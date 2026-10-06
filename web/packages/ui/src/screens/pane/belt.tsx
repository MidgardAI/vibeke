// Action belt (spec 16 §9.1): Keys (sticky modifiers, chord mode, echo ✓), Quick replies, Agent
// slash commands, Display.

import { useState, type ReactNode } from 'react';
import { Check, Minus, Plus, Send, Trash2, WrapText } from 'lucide-react';
import { useApp, usePrefs } from '../../app/hooks';
import { Button, Segmented, Toggle, cx } from '../../components/ui';
import { t } from '../../i18n';
import { quickRepliesFor, slashCommandsFor, type SlashCommand } from '../../lib/harness';
import { NO_MODS, cycleMod, keyLabel, press, queueAdd, queueRemoveAt, type Modifier, type Mods } from '../../lib/keys';
import type { PaneActions } from './actions';

export type BeltTab = 'keys' | 'quick' | 'agent' | 'display';

const KEYPAD: { key: string; label?: string }[][] = [
  [{ key: 'esc' }, { key: 'tab' }, { key: 'shift+tab', label: '⇧⇥' }, { key: 'up' }, { key: 'ctrl+c', label: '^C' }, { key: 'ctrl+d', label: '^D' }],
  [{ key: 'space' }, { key: 'left' }, { key: 'down' }, { key: 'right' }, { key: 'backspace' }, { key: 'enter' }],
];

const H = { s: 'h-9', m: 'h-11', l: 'h-13' } as const;

export function ActionBelt({
  tab,
  setTab,
  actions,
  harness,
  canType,
  onInsert,
  zen,
  setZen,
}: {
  tab: BeltTab | null;
  setTab(t: BeltTab | null): void;
  actions: PaneActions;
  harness: string | null;
  canType: boolean;
  onInsert(text: string): void;
  zen: boolean;
  setZen(v: boolean): void;
}) {
  const tabs: { id: BeltTab; label: string; hide?: boolean }[] = [
    { id: 'keys', label: t.belt.keys, hide: !canType },
    { id: 'quick', label: t.belt.quick, hide: !canType },
    { id: 'agent', label: t.belt.agent, hide: !canType || !harness },
    { id: 'display', label: t.belt.display },
  ];
  return (
    <div className="border-t border-border bg-surface">
      {tab === 'keys' && <KeysPanel actions={actions} />}
      {tab === 'quick' && <QuickPanel actions={actions} harness={harness} />}
      {tab === 'agent' && <AgentPanel actions={actions} harness={harness} onInsert={onInsert} />}
      {tab === 'display' && <DisplayPanel zen={zen} setZen={setZen} />}
      <div className="flex gap-1 px-2 py-1">
        {tabs
          .filter((x) => !x.hide)
          .map((x) => (
            <button
              key={x.id}
              type="button"
              onClick={() => setTab(tab === x.id ? null : x.id)}
              className={cx(
                'h-8 flex-1 rounded-lg border border-transparent text-[13px] font-medium',
                tab === x.id ? 'bg-accent/15 text-accent' : 'text-muted',
              )}
            >
              {x.label}
            </button>
          ))}
      </div>
    </div>
  );
}

function KeyButton({ children, onClick, active, locked, h }: { children: ReactNode; onClick(): void; active?: boolean; locked?: boolean; h: string }) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={cx(
        'min-w-0 flex-1 rounded-lg border font-mono text-[13px] active:bg-surface-2',
        h,
        locked ? 'border-accent bg-accent text-accent-fg' : active ? 'border-accent bg-accent/15 text-accent' : 'border-border bg-bg text-fg',
      )}
    >
      {children}
    </button>
  );
}

function KeysPanel({ actions }: { actions: PaneActions }) {
  const app = useApp();
  const prefs = usePrefs();
  const [mods, setMods] = useState<Mods>(NO_MODS);
  const [chord, setChord] = useState(false);
  const [queue, setQueue] = useState<{ keys: string[] }>({ keys: [] });
  const [echo, setEcho] = useState<{ label: string; ok: boolean | null } | null>(null);
  const [char, setChar] = useState('');
  const h = H[prefs.beltSize];

  const hit = async (base: string) => {
    // Keypad entries that already carry a modifier (shift+tab, ctrl+c) are sent as-is.
    const r = /^[a-z]+\+/.test(base) ? { key: base, mods } : press(base, mods);
    setMods(r.mods);
    app.haptic('tap');
    if (chord) {
      setQueue((q) => queueAdd(q, r.key));
      return;
    }
    setEcho({ label: keyLabel(r.key), ok: null });
    const ok = await actions.keys([r.key]);
    setEcho({ label: keyLabel(r.key), ok });
    setTimeout(() => setEcho((e) => (e && e.label === keyLabel(r.key) ? null : e)), 1200);
  };

  const mod = (m: Modifier) => setMods((cur) => ({ ...cur, [m]: cycleMod(cur[m]) }));

  return (
    <div className="space-y-1.5 px-2 pt-2">
      {chord && (
        <div className="flex min-h-9 items-center gap-1.5 overflow-x-auto no-scrollbar">
          {queue.keys.length === 0 && <span className="text-[12px] text-faint">{t.belt.chordHint}</span>}
          {queue.keys.map((k, i) => (
            <button key={i} type="button" className="h-7 shrink-0 rounded-md bg-surface-2 px-2 font-mono text-[12px]" onClick={() => setQueue((q) => queueRemoveAt(q, i))}>
              {keyLabel(k)}
            </button>
          ))}
          <span className="flex-1" />
          {queue.keys.length > 0 && (
            <>
              <Button size="sm" variant="ghost" icon={<Trash2 className="size-3.5" />} onClick={() => setQueue({ keys: [] })}>
                {t.belt.clearQueue}
              </Button>
              <Button
                size="sm"
                variant="primary"
                icon={<Send className="size-3.5" />}
                onClick={async () => {
                  const keys = queue.keys;
                  setQueue({ keys: [] });
                  const ok = await actions.keys(keys);
                  setEcho({ label: keys.map(keyLabel).join(' '), ok });
                }}
              >
                {t.belt.sendKeys}
              </Button>
            </>
          )}
        </div>
      )}
      {KEYPAD.map((row, i) => (
        <div key={i} className="flex gap-1.5">
          {row.map((k) => (
            <KeyButton key={k.key} h={h} onClick={() => void hit(k.key)}>
              {k.label ?? keyLabel(k.key)}
            </KeyButton>
          ))}
        </div>
      ))}
      <div className="flex gap-1.5">
        {(['ctrl', 'alt', 'shift'] as Modifier[]).map((m) => (
          <KeyButton key={m} h={h} active={mods[m] === 'once'} locked={mods[m] === 'locked'} onClick={() => mod(m)}>
            {keyLabel(m)} {m}
          </KeyButton>
        ))}
        <input
          aria-label="key"
          value={char}
          maxLength={1}
          autoCapitalize="off"
          autoCorrect="off"
          placeholder="a"
          onChange={(e) => {
            const c = e.target.value.slice(-1);
            setChar('');
            if (c) void hit(c === ' ' ? 'space' : c);
          }}
          className={cx('w-12 rounded-lg border border-border bg-bg text-center font-mono text-[13px]', h)}
        />
        <KeyButton h={h} active={chord} onClick={() => setChord(!chord)}>
          {t.belt.chord}
        </KeyButton>
      </div>
      <div className="h-5 text-center text-[12px] text-muted">
        {echo && (
          <span className="inline-flex items-center gap-1 font-mono">
            {echo.label} {echo.ok === true && <Check className="size-3.5 text-ok" />}
            {echo.ok === false && <span className="text-danger">✕</span>}
          </span>
        )}
      </div>
    </div>
  );
}

function QuickPanel({ actions, harness }: { actions: PaneActions; harness: string | null }) {
  const prefs = usePrefs();
  const app = useApp();
  const replies = quickRepliesFor(harness, prefs.quickReplies);
  const [tapped, setTapped] = useState<string | null>(null);
  return (
    <div className="px-2 pt-2">
      <div className="flex flex-wrap gap-1.5">
        {replies.map((r) => (
          <button
            key={r}
            type="button"
            onClick={async () => {
              app.haptic('tap');
              setTapped(r);
              await actions.text(r);
              setTimeout(() => setTapped(null), 1500);
            }}
            className={cx(
              'inline-flex h-9 items-center gap-1 rounded-full border border-border bg-bg px-3 text-[13px]',
              tapped && tapped !== r && 'opacity-40',
            )}
          >
            {tapped === r && <Check className="size-3.5 text-ok" />}
            {r}
          </button>
        ))}
      </div>
      <div className="py-1.5 text-[11px] text-faint">{t.belt.editQuick}</div>
    </div>
  );
}

function AgentPanel({ actions, harness, onInsert }: { actions: PaneActions; harness: string | null; onInsert(text: string): void }) {
  const app = useApp();
  const cmds = slashCommandsFor(harness);
  const [armed, setArmed] = useState<string | null>(null);
  const [sent, setSent] = useState<string | null>(null);
  if (!cmds.length) return <div className="px-3 py-3 text-[13px] text-muted">{t.belt.noCommands}</div>;
  const tap = async (c: SlashCommand) => {
    if (c.takesArg) return onInsert(`${c.command} `);
    if (c.dangerous && armed !== c.command) {
      setArmed(c.command);
      app.haptic('warning');
      return;
    }
    setArmed(null);
    app.haptic('tap');
    if (await actions.text(c.command)) {
      setSent(c.command);
      setTimeout(() => setSent(null), 1500);
    }
  };
  return (
    <div className="max-h-56 overflow-y-auto px-2 pt-1">
      {cmds.map((c) => (
        <button key={c.command} type="button" onClick={() => void tap(c)} className="flex w-full items-center gap-2 rounded-lg px-2 py-1.5 text-left active:bg-surface-2">
          <span className={cx('font-mono text-[13px]', c.dangerous ? 'text-danger' : 'text-accent')}>{c.command}</span>
          <span className="min-w-0 flex-1 truncate text-[12px] text-muted">{armed === c.command ? t.composer.tapAgain : c.description}</span>
          {sent === c.command && <Check className="size-3.5 text-ok" />}
        </button>
      ))}
    </div>
  );
}

function DisplayPanel({ zen, setZen }: { zen: boolean; setZen(v: boolean): void }) {
  const app = useApp();
  const prefs = usePrefs();
  return (
    <div className="space-y-2 px-3 py-2.5">
      <div className="flex items-center justify-between">
        <span className="flex items-center gap-2 text-sm">
          <WrapText className="size-4 text-muted" /> {t.pane.wrap}
        </span>
        <Toggle label={t.pane.wrap} checked={prefs.wrap} onChange={(v) => app.prefs.patch({ wrap: v })} />
      </div>
      <div className="flex items-center justify-between">
        <span className="text-sm">{t.pane.textSize}</span>
        <div className="flex items-center gap-2">
          <Button size="sm" variant="outline" aria-label="smaller" onClick={() => app.prefs.patch({ termFont: Math.max(8, prefs.termFont - 1) })}>
            <Minus className="size-4" />
          </Button>
          <span className="w-8 text-center text-sm tabular-nums">{prefs.termFont}</span>
          <Button size="sm" variant="outline" aria-label="larger" onClick={() => app.prefs.patch({ termFont: Math.min(24, prefs.termFont + 1) })}>
            <Plus className="size-4" />
          </Button>
        </div>
      </div>
      <div className="flex items-center justify-between">
        <span className="text-sm">{t.settings.beltSize}</span>
        <Segmented
          label={t.settings.beltSize}
          value={prefs.beltSize}
          onChange={(v) => app.prefs.patch({ beltSize: v })}
          options={(['s', 'm', 'l'] as const).map((v) => ({ value: v, label: t.settings.sizes[v]! }))}
        />
      </div>
      <div className="flex items-center justify-between">
        <span className="text-sm">{t.pane.zen}</span>
        <Toggle label={t.pane.zen} checked={zen} onChange={setZen} />
      </div>
    </div>
  );
}
