// The find bar of the terminal and the conversation: a text field, "n/m", previous / next and
// close. Enter steps forward, Shift+Enter backward, Escape closes. `data-find-input` lets the
// find shortcut focus it (app/keyboard.tsx).

import { ChevronDown, ChevronUp, Search, X } from 'lucide-react';
import { t } from '../i18n';
import { IconButton } from './ui';

export function FindBar({
  query,
  onQuery,
  index,
  count,
  onStep,
  onClose,
  placeholder,
  note,
}: {
  query: string;
  onQuery(q: string): void;
  /** Current hit, 0-based (ignored without hits). */
  index: number;
  count: number;
  onStep(dir: 1 | -1): void;
  onClose(): void;
  placeholder: string;
  /** A line under the bar (what is searched, or that older messages load). */
  note?: string | null;
}) {
  return (
    <div className="border-b border-border bg-surface" data-find-bar>
      <div className="flex items-center gap-1 px-2 py-1">
        <Search className="size-4 text-muted" />
        <input
          autoFocus
          data-find-input
          value={query}
          onChange={(e) => onQuery(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && count) {
              e.preventDefault();
              onStep(e.shiftKey ? -1 : 1);
            } else if (e.key === 'Escape') {
              e.preventDefault();
              onClose();
            }
          }}
          placeholder={placeholder}
          enterKeyHint="search"
          className="h-8 min-w-0 flex-1 bg-transparent text-base outline-none sm:text-sm"
        />
        <span className="text-xs tabular-nums text-muted" aria-live="polite">
          {query ? `${count ? index + 1 : 0}/${count}` : ''}
        </span>
        <IconButton label={t.conv.findPrev} disabled={!count} onClick={() => onStep(-1)}>
          <ChevronUp />
        </IconButton>
        <IconButton label={t.conv.findNext} disabled={!count} onClick={() => onStep(1)}>
          <ChevronDown />
        </IconButton>
        <IconButton label={t.close} onClick={onClose}>
          <X />
        </IconButton>
      </div>
      {note && <div className="px-3 pb-1 text-xs text-faint">{note}</div>}
    </div>
  );
}
