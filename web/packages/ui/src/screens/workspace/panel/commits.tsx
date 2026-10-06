// The Commits section at the bottom of the Changes tab: recent `git.log` entries; a click shows
// that commit's files (`?commit=`). Hidden when the host does not know `git.log`.

import { useEffect, useState } from 'react';
import { ChevronRight } from 'lucide-react';
import type { Commit } from '@vibeke/core';
import { useApp } from '../../../app/hooks';
import { RelTime, cx } from '../../../components/ui';
import { t } from '../../../i18n';
import { errorMessage } from '../../../lib/answer';
import { noteUnsupported, supported } from '../../../lib/supports';

export const LOG_LIMIT = 50;

export interface GitLogState {
  commits: Commit[] | null;
  truncated: boolean;
  unsupported: boolean;
  error: string | null;
}

/** Recent commits of the pane's repo; refetched when `reload` changes. */
export function useGitLog(host: string, pane: string | null, reload: unknown): GitLogState {
  const app = useApp();
  const [s, setS] = useState<GitLogState>({ commits: null, truncated: false, unsupported: !supported(host, 'git.log'), error: null });
  useEffect(() => {
    if (!pane || !supported(host, 'git.log')) return;
    let live = true;
    app
      .conn(host)
      ?.request('git.log', { pane, limit: LOG_LIMIT })
      .then(
        (r) => live && setS({ commits: r.commits, truncated: r.truncated, unsupported: false, error: null }),
        (e) => {
          if (!live) return;
          if (noteUnsupported(host, 'git.log', e)) setS((p) => ({ ...p, unsupported: true }));
          else setS((p) => ({ ...p, error: errorMessage(e) }));
        },
      );
    return () => {
      live = false;
    };
  }, [app, host, pane, reload]);
  return s;
}

/** The commit has no parent in view: the last of a complete (untruncated, short) log. */
export function isRootCommit(log: GitLogState, sha: string): boolean {
  const c = log.commits;
  if (!c || log.truncated || c.length >= LOG_LIMIT) return false;
  return c[c.length - 1]?.sha === sha;
}

export function CommitsSection({
  log,
  open,
  onToggle,
  selected,
  onSelect,
}: {
  log: GitLogState;
  open: boolean;
  onToggle(): void;
  selected: string | null;
  onSelect(sha: string): void;
}) {
  if (log.unsupported) return null;
  const commits = log.commits ?? [];
  return (
    <section aria-label={t.panel.commits} className={cx('flex shrink-0 flex-col border-t border-border', open && 'max-h-[42%] min-h-0')}>
      <button type="button" aria-expanded={open} onClick={onToggle} className="vk-focus flex h-9 shrink-0 items-center gap-1.5 px-2 text-left text-sm text-fg/90 hover:bg-hover pointer-coarse:h-10">
        <span className="flex size-4 items-center justify-center text-faint">
          <ChevronRight aria-hidden className={cx('size-3.5 transition-transform', open && 'rotate-90')} />
        </span>
        <span className="flex-1">{t.panel.commits}</span>
        {log.commits && <span className="text-xs tabular-nums text-faint">{log.truncated || commits.length >= LOG_LIMIT ? `${commits.length}+` : commits.length}</span>}
      </button>
      {open && (
        <div role="list" className="min-h-0 flex-1 overflow-y-auto pb-1">
          {log.error && <div className="px-3 py-2 text-xs text-del">{log.error}</div>}
          {log.commits && commits.length === 0 && <div className="px-3 py-2 text-xs text-muted">{t.panel.noCommits}</div>}
          {commits.map((c) => {
            const on = c.sha === selected;
            return (
              <button
                key={c.sha}
                type="button"
                role="listitem"
                aria-current={on ? 'true' : undefined}
                title={`${c.short} · ${c.author}\n${c.subject}`}
                onClick={() => onSelect(c.sha)}
                className={cx('vk-focus flex h-[var(--row-h)] w-full min-w-0 items-center gap-2 pl-8 pr-3 text-left text-sm pointer-coarse:h-9', on ? 'bg-selected text-fg' : 'text-fg/90 hover:bg-hover')}
              >
                <span className="min-w-0 flex-1 truncate">{c.subject}</span>
                <span className="shrink-0 font-mono text-2xs text-faint">{c.short}</span>
                <RelTime ms={c.ts} className="w-7 shrink-0 text-right text-xs text-faint" />
              </button>
            );
          })}
        </div>
      )}
    </section>
  );
}
