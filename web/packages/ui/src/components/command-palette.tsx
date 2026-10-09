// Command palette (spec 16 §16.2, ⌘K / Ctrl+K): jump to any host, workspace, pane or open
// interaction, and run actions (new agent, share, hand off, pair, settings). Shared by the
// desktop app and the PWA.

import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { Bot, Command, FolderGit2, Inbox, Search, Server, SquareTerminal } from 'lucide-react';
import { displayName } from '@vibeke/core';
import { useHosts, useInboxItems, useTree } from '../app/hooks';
import { t } from '../i18n';
import { harnessLabel } from '../lib/harness';
import { fuzzyScore } from '../lib/shortcuts';
import { Dialog } from './dialog';
import { cx } from './ui';

export interface PaletteItem {
  id: string;
  group: 'command' | 'pane' | 'workspace' | 'host' | 'interaction';
  title: string;
  sub?: string;
  keywords?: string;
  shortcut?: string;
  run(): void;
}

const ICONS: Record<PaletteItem['group'], ReactNode> = {
  command: <Command className="size-4" />,
  pane: <SquareTerminal className="size-4" />,
  workspace: <FolderGit2 className="size-4" />,
  host: <Server className="size-4" />,
  interaction: <Inbox className="size-4" />,
};

/** Entities from the live dashboards as palette items; `go` performs the navigation. */
export function useEntityItems(go: {
  pane(host: string, pane: string): void;
  workspace(host: string, workspace: string): void;
  host(host: string): void;
  interaction(host: string, id: string): void;
}): PaletteItem[] {
  const tree = useTree();
  const hosts = useHosts();
  const inbox = useInboxItems();
  return useMemo(() => {
    const out: PaletteItem[] = [];
    const multi = hosts.length > 1;
    for (const it of inbox) {
      const i = it.interaction;
      out.push({
        id: `i:${it.host_id}/${i.id}`,
        group: 'interaction',
        title: i.action?.command ?? i.title,
        sub: [t.palette.answer, harnessLabel(i.harness ?? it.run?.harness), it.pane?.title ?? it.pane?.auto_title].filter(Boolean).join(' · '),
        run: () => go.interaction(it.host_id, i.id),
      });
    }
    for (const r of tree.all) {
      out.push({
        id: `p:${r.key}`,
        group: 'pane',
        title: r.pane.title ?? r.run?.name ?? r.run?.title ?? r.pane.auto_title,
        sub: [r.run ? harnessLabel(r.run.harness) : t.palette.pane, r.workspace ? displayName(r.workspace) : null, multi ? r.hostName : null].filter(Boolean).join(' · '),
        keywords: [r.pane.handle, r.run?.cwd ?? r.pane.cwd ?? ''].join(' '),
        run: () => go.pane(r.host, r.pane.id),
      });
    }
    for (const g of tree.hosts) {
      const hid = g.host.record.host_id;
      const name = g.host.info?.host_name ?? g.host.record.name;
      for (const w of g.workspaces) {
        out.push({
          id: `w:${hid}/${w.workspace.id}`,
          group: 'workspace',
          title: displayName(w.workspace),
          sub: [t.palette.workspace, w.workspace.branch ? `⎇ ${w.workspace.branch}` : null, multi ? name : null].filter(Boolean).join(' · '),
          keywords: w.workspace.root_path,
          run: () => go.workspace(hid, w.workspace.id),
        });
      }
      out.push({ id: `h:${hid}`, group: 'host', title: name, sub: `${t.palette.host} · ${g.host.status}`, run: () => go.host(hid) });
    }
    return out;
  }, [tree, hosts, inbox]);
}

export function CommandPalette({ open, onClose, items }: { open: boolean; onClose(): void; items: PaletteItem[] }) {
  const [q, setQ] = useState('');
  const [sel, setSel] = useState(0);
  const input = useRef<HTMLInputElement>(null);
  const list = useRef<HTMLDivElement>(null);

  // Fresh on every open (focus moves in and back out through the Dialog).
  useEffect(() => {
    if (!open) return;
    setQ('');
    setSel(0);
  }, [open]);

  const shown = useMemo(() => {
    if (!q.trim()) {
      // Empty query: commands and open interactions first, then panes.
      const order: PaletteItem['group'][] = ['interaction', 'command', 'pane', 'workspace', 'host'];
      return [...items].sort((a, b) => order.indexOf(a.group) - order.indexOf(b.group)).slice(0, 60);
    }
    return items
      .map((it) => ({ it, s: Math.max(fuzzyScore(q, it.title), fuzzyScore(q, `${it.title} ${it.sub ?? ''} ${it.keywords ?? ''}`) - 30) }))
      .filter((x) => x.s >= 0)
      .sort((a, b) => b.s - a.s)
      .slice(0, 60)
      .map((x) => x.it);
  }, [items, q]);

  useEffect(() => setSel(0), [q]);
  useEffect(() => {
    list.current?.querySelector(`[data-index="${sel}"]`)?.scrollIntoView({ block: 'nearest' });
  }, [sel]);

  if (!open) return null;
  const choose = (it: PaletteItem | undefined) => {
    if (!it) return;
    onClose();
    it.run();
  };
  const activeId = shown[sel] ? `palette-${sel}` : undefined;

  return (
    <Dialog
      open
      onClose={onClose}
      label={t.palette.label}
      initialFocus={input}
      className="fixed inset-0 z-[80] flex items-start justify-center px-4 pt-[12vh]"
      panelClassName="animate-pop relative flex max-h-[70vh] w-full max-w-xl flex-col overflow-hidden rounded-2xl border border-border bg-surface shadow-2xl outline-none"
    >
        <div className="flex items-center gap-2 border-b border-border px-3.5">
          <Search className="size-4 shrink-0 text-muted" aria-hidden />
          <input
            ref={input}
            value={q}
            onChange={(e) => setQ(e.target.value)}
            placeholder={t.palette.placeholder}
            role="combobox"
            aria-label={t.palette.placeholder}
            aria-autocomplete="list"
            aria-expanded="true"
            aria-controls="palette-list"
            aria-activedescendant={activeId}
            autoComplete="off"
            spellCheck={false}
            className="h-12 min-w-0 flex-1 bg-transparent text-base text-fg outline-none placeholder:text-faint"
            onKeyDown={(e) => {
              if (e.key === 'ArrowDown' || (e.ctrlKey && e.key === 'n')) {
                e.preventDefault();
                setSel((s) => Math.min(shown.length - 1, s + 1));
              } else if (e.key === 'ArrowUp' || (e.ctrlKey && e.key === 'p')) {
                e.preventDefault();
                setSel((s) => Math.max(0, s - 1));
              } else if (e.key === 'Enter') {
                e.preventDefault();
                choose(shown[sel]);
              }
            }}
          />
        </div>
        <div ref={list} id="palette-list" role="listbox" aria-label={t.palette.label} className="min-h-0 flex-1 overflow-y-auto p-1.5">
          {shown.length === 0 && <div className="px-3 py-6 text-center text-sm text-muted">{t.palette.empty}</div>}
          {shown.map((it, i) => (
            <div
              key={it.id}
              id={`palette-${i}`}
              data-index={i}
              role="option"
              aria-selected={i === sel}
              onMouseMove={() => i !== sel && setSel(i)}
              onClick={() => choose(it)}
              className={cx('flex min-h-11 cursor-default items-center gap-3 rounded-xl px-2.5 py-1.5', i === sel && 'bg-accent/12')}
            >
              <span className={cx('shrink-0', i === sel ? 'text-accent' : 'text-muted')}>{it.group === 'pane' && it.sub && !it.sub.startsWith(t.palette.pane) ? <Bot className="size-4" /> : ICONS[it.group]}</span>
              <span className="min-w-0 flex-1">
                <span className="block truncate text-sm">{it.title}</span>
                {it.sub && <span className="block truncate text-xs text-muted">{it.sub}</span>}
              </span>
              {it.shortcut && <kbd className="shrink-0 rounded-md border border-border px-1.5 font-sans text-2xs text-muted">{it.shortcut}</kbd>}
            </div>
          ))}
        </div>
        <div className="border-t border-border px-3.5 py-1.5 text-2xs text-muted">{t.palette.hint}</div>
    </Dialog>
  );
}
