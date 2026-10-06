// A pane's terminal as a workspace tab: the polled screen mirror with find (/ or ⌘F), an offline
// note with the last screen's age, and the password-prompt warning for the composer.

import { useEffect, useMemo, useState } from 'react';
import { ChevronDown, ChevronUp, Search, X } from 'lucide-react';
import { useHost, useNow, usePrefs } from '../../app/hooks';
import { countMatches, TerminalMirror } from '../../components/terminal';
import { IconButton, Notice } from '../../components/ui';
import { t } from '../../i18n';
import { stripAnsi } from '../../lib/ansi';
import { ago } from '../../lib/format';
import { isNoEchoPrompt } from '../../lib/guards';
import { useMirror } from '../pane/use-mirror';

export function TerminalTab({
  hostId,
  pane,
  working,
  findOpen,
  setFindOpen,
  onNoEcho,
  burstRef,
}: {
  hostId: string;
  pane: string;
  working: boolean;
  findOpen: boolean;
  setFindOpen(v: boolean): void;
  /** The screen shows a password prompt (the composer warns). */
  onNoEcho(v: boolean): void;
  /** Filled with the mirror's burst (fast polls after a send). */
  burstRef: { current: (() => void) | null };
}) {
  const prefs = usePrefs();
  const host = useHost(hostId);
  const now = useNow(5000);
  const online = host?.status === 'online';
  const mirror = useMirror(hostId, pane, working);
  const [query, setQuery] = useState('');
  const [hit, setHit] = useState(0);
  const plain = useMemo(() => stripAnsi(mirror.text), [mirror.text]);
  const matches = useMemo(() => countMatches(plain, query), [plain, query]);
  const noEcho = useMemo(() => isNoEchoPrompt(plain), [plain]);

  useEffect(() => {
    burstRef.current = mirror.burst;
    return () => {
      burstRef.current = null;
    };
  }, [mirror.burst]);
  useEffect(() => onNoEcho(noEcho), [noEcho]);
  useEffect(() => () => onNoEcho(false), []);

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      {findOpen && (
        <div className="flex items-center gap-1 border-b border-border bg-surface px-2 py-1">
          <Search className="size-4 text-muted" />
          <input
            autoFocus
            data-find-input
            value={query}
            onChange={(e) => {
              setQuery(e.target.value);
              setHit(0);
            }}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && matches) setHit((h) => (e.shiftKey ? (h - 1 + matches) % matches : (h + 1) % matches));
            }}
            placeholder={t.pane.findPlaceholder}
            className="h-8 min-w-0 flex-1 bg-transparent text-sm outline-none"
          />
          <span className="text-xs tabular-nums text-muted">{query ? `${matches ? hit + 1 : 0}/${matches}` : ''}</span>
          <IconButton label="previous" disabled={!matches} onClick={() => setHit((h) => (h - 1 + matches) % matches)}>
            <ChevronUp />
          </IconButton>
          <IconButton label="next" disabled={!matches} onClick={() => setHit((h) => (h + 1) % matches)}>
            <ChevronDown />
          </IconButton>
          <IconButton
            label={t.close}
            onClick={() => {
              setFindOpen(false);
              setQuery('');
            }}
          >
            <X />
          </IconButton>
        </div>
      )}
      {!online && mirror.at && <Notice tone="warn" className="m-2">{t.pane.offlineMirror(ago(mirror.at, now))}</Notice>}
      <div className="relative min-h-0 flex-1">
        {mirror.text ? (
          <TerminalMirror
            text={mirror.text}
            lines={mirror.lines}
            wrap={prefs.wrap}
            fontSize={prefs.termFont}
            find={query ? { query, current: hit } : undefined}
            className="absolute inset-0"
          />
        ) : (
          <div className="term absolute inset-0 flex items-center justify-center text-sm text-faint">{online ? t.loading : t.pane.noMirror}</div>
        )}
      </div>
    </div>
  );
}
