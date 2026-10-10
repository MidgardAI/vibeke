// A pane's terminal as a workspace tab: the polled screen mirror with find (/ or ⌘F), an offline
// note with the last screen's age, and the password-prompt warning for the composer. Clicking the
// screen (without selecting text) hands typing focus to the composer.

import { useEffect, useMemo, useRef, useState } from 'react';
import { ArrowDown } from 'lucide-react';
import { useHost, useNow, usePrefs } from '../../app/hooks';
import { FindBar } from '../../components/find-bar';
import { countMatches, TerminalMirror } from '../../components/terminal';
import { Notice } from '../../components/ui';
import { t } from '../../i18n';
import { stripAnsi } from '../../lib/ansi';
import { plainOutput } from '../../lib/copy-output';
import { ago } from '../../lib/format';
import { isNoEchoPrompt } from '../../lib/guards';
import { stepHit } from '../../lib/conv-find';
import { useMirror } from '../pane/use-mirror';
import { usePathLinks } from './use-path-links';

export function TerminalTab({
  hostId,
  pane,
  working,
  findOpen,
  setFindOpen,
  onNoEcho,
  burstRef,
  onActivate,
  cwd,
  openFile,
  copyRef,
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
  /** A click on the screen that selected nothing: focus the composer. */
  onActivate?: () => void;
  /** The pane's directory, to resolve relative paths in the output. */
  cwd?: string | null;
  /** Open a workspace file in the viewer; without it paths in the output are not links. */
  openFile?: (path: string, line?: number) => void;
  /** Filled with a function returning the visible text as plain text (Copy output). */
  copyRef?: { current: (() => string) | null };
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
  const links = usePathLinks(hostId, pane, cwd, plain, openFile);
  const [atBottom, setAtBottom] = useState(true);
  const jumpRef = useRef<(() => void) | null>(null);
  const plainRef = useRef(plain);
  plainRef.current = plain;
  useEffect(() => {
    if (!copyRef) return;
    copyRef.current = () => plainOutput(plainRef.current);
    return () => {
      copyRef.current = null;
    };
  }, [copyRef]);

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
        <FindBar
          query={query}
          onQuery={(q) => {
            setQuery(q);
            setHit(0);
          }}
          index={hit}
          count={matches}
          onStep={(dir) => setHit((h) => stepHit(h, matches, dir))}
          onClose={() => {
            setFindOpen(false);
            setQuery('');
          }}
          placeholder={t.pane.findPlaceholder}
        />
      )}
      {!online && mirror.at && <Notice tone="warn" className="m-2">{t.pane.offlineMirror(ago(mirror.at, now))}</Notice>}
      <div
        className="relative min-h-0 flex-1"
        data-terminal-screen
        onMouseUp={(e) => {
          if (!onActivate || e.button !== 0) return;
          if ((e.target as HTMLElement).closest?.('[data-link]')) return;
          const sel = window.getSelection?.();
          if (sel && !sel.isCollapsed && sel.toString()) return;
          onActivate();
        }}
      >
        {mirror.text ? (
          <TerminalMirror
            text={mirror.text}
            lines={mirror.lines}
            wrap={prefs.wrap}
            fontSize={prefs.termFont}
            find={query ? { query, current: hit } : undefined}
            links={links}
            onAtBottom={setAtBottom}
            jumpRef={jumpRef}
            className="absolute inset-0"
          />
        ) : (
          <div className="term absolute inset-0 flex items-center justify-center text-sm text-faint">{online ? t.loading : t.pane.noMirror}</div>
        )}
        {!atBottom && mirror.text && (
          <button
            type="button"
            aria-label={t.pane.jumpLatest}
            title={t.pane.jumpLatest}
            onClick={() => jumpRef.current?.()}
            className="vk-focus absolute bottom-3 left-1/2 inline-flex size-9 -translate-x-1/2 items-center justify-center rounded-full border border-border bg-surface-2 text-muted shadow-[var(--shadow)] hover:text-fg pointer-coarse:size-11"
          >
            <ArrowDown className="size-4" />
          </button>
        )}
      </div>
    </div>
  );
}
