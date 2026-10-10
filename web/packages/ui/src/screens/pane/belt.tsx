// Action belt (spec 16 §9.1): Keys (sticky modifiers, chord mode, echo ✓), Quick replies, Agent
// slash commands, Display.

import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { Check, Minus, Pencil, Plus, Send, Trash2, WrapText } from 'lucide-react';
import { useApp, usePrefs } from '../../app/hooks';
import { Button, Segmented, Toggle, cx } from '../../components/ui';
import { t } from '../../i18n';
import { quickRepliesFor, slashCommandsFor, type SlashCommand } from '../../lib/harness';
import { HoldRepeater } from '../../lib/hold-repeat';
import { DEFAULT_LAYOUT, FUNCTION_KEYS, isDangerous, isRepeatable, packRows, padId, padLabel, resolvePad, type PlacedKey } from '../../lib/key-layout';
import { NO_MODS, cycleMod, keyLabel, queueAdd, queueRemoveAt, type Modifier, type Mods } from '../../lib/keys';
import type { PaneActions } from './actions';
import { KeysEditor } from './keys-editor';

export type BeltTab = 'keys' | 'quick' | 'agent' | 'display';

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
  /** Zen (screen only) where the host screen offers it; omitted = no Zen row. */
  zen?: boolean;
  setZen?(v: boolean): void;
}) {
  const prefs = usePrefs();
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
      <div className={cx('flex gap-1 px-2 py-1', prefs.leftHand && 'flex-row-reverse')}>
        {tabs
          .filter((x) => !x.hide)
          .map((x) => (
            <button
              key={x.id}
              type="button"
              onClick={() => setTab(tab === x.id ? null : x.id)}
              className={cx(
                'h-8 flex-1 rounded-lg border border-transparent text-sm font-medium',
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
        'min-w-0 flex-1 rounded-lg border font-mono text-sm active:bg-surface-2',
        h,
        locked ? 'border-accent bg-accent text-accent-fg' : active ? 'border-accent bg-accent/15 text-accent' : 'border-border bg-bg text-fg',
      )}
    >
      {children}
    </button>
  );
}

function PadKeyButton({
  placed,
  h,
  armed,
  repeat,
  send,
  onTap,
}: {
  placed: PlacedKey;
  h: string;
  armed: boolean;
  /** Arrow keys repeat while held (not in chord mode). */
  repeat: boolean;
  /** One press, resolving to whether it was sent (repeating stops on a failure). */
  send(): Promise<boolean>;
  onTap(): void;
}) {
  const sendRef = useRef(send);
  sendRef.current = send;
  const rep = useMemo(() => new HoldRepeater(() => sendRef.current()), []);
  const viaPointer = useRef(false);
  useEffect(() => () => rep.stop(), [rep]);
  const label = padLabel(placed.key);
  return (
    <button
      type="button"
      aria-label={armed ? `${label}. ${t.belt.tapAgainKey}` : undefined}
      style={{ gridColumn: `${placed.start} / span ${placed.span}`, touchAction: 'manipulation' }}
      onPointerDown={(e) => {
        if (!repeat || (e.pointerType === 'mouse' && e.button !== 0)) return;
        viaPointer.current = true;
        rep.start();
      }}
      onPointerUp={() => rep.stop()}
      onPointerCancel={() => {
        viaPointer.current = false;
        rep.stop();
      }}
      onPointerLeave={() => {
        if (rep.active) viaPointer.current = false;
        rep.stop();
      }}
      onContextMenu={repeat ? (e) => e.preventDefault() : undefined}
      onClick={() => {
        // A repeating key was already sent on press; the click that follows only ends the press.
        if (viaPointer.current) {
          viaPointer.current = false;
          return;
        }
        onTap();
      }}
      className={cx(
        'min-w-0 select-none truncate rounded-lg border font-mono text-sm active:bg-surface-2',
        h,
        armed ? 'border-danger bg-danger/15 text-danger' : 'border-border bg-bg text-fg',
      )}
    >
      {label}
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
  const [fKeys, setFKeys] = useState(false);
  const [editing, setEditing] = useState(false);
  const [armed, setArmed] = useState<string | null>(null);
  const armTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const modsRef = useRef(mods);
  modsRef.current = mods;
  const h = H[prefs.beltSize];
  const left = prefs.leftHand;
  const keys = fKeys ? FUNCTION_KEYS : (prefs.keyLayout ?? DEFAULT_LAYOUT).keys;
  const rows = useMemo(() => packRows(keys, left), [keys, left]);
  useEffect(
    () => () => {
      if (armTimer.current) clearTimeout(armTimer.current);
    },
    [],
  );

  const arm = (id: string | null) => {
    if (armTimer.current) clearTimeout(armTimer.current);
    setArmed(id);
    if (id) armTimer.current = setTimeout(() => setArmed(null), 3000);
  };

  const sendKeys = async (list: string[]): Promise<boolean> => {
    const label = list.map(keyLabel).join(' ');
    setEcho({ label, ok: null });
    const ok = await actions.keys(list);
    setEcho({ label, ok });
    setTimeout(() => setEcho((e) => (e && e.label === label ? null : e)), 1200);
    return ok;
  };

  /** Press a pad key; resolves to whether it was sent. A dangerous key only arms on the first tap. */
  const hit = async (key: { steps: string[] }, id: string): Promise<boolean> => {
    if (!chord && isDangerous(key) && armed !== id) {
      arm(id);
      app.haptic('warning');
      return false;
    }
    arm(null);
    const r = resolvePad(key, modsRef.current);
    setMods(r.mods);
    modsRef.current = r.mods;
    app.haptic('tap');
    if (chord) {
      setQueue((q) => r.keys.reduce(queueAdd, q));
      return true;
    }
    return sendKeys(r.keys);
  };

  const mod = (m: Modifier) => setMods((cur) => ({ ...cur, [m]: cycleMod(cur[m]) }));
  const queueDanger = isDangerous({ steps: queue.keys });

  return (
    <div className="space-y-1.5 px-2 pt-2">
      {chord && (
        <div className={cx('flex min-h-9 items-center gap-1.5 overflow-x-auto no-scrollbar', left && 'flex-row-reverse')}>
          {queue.keys.length === 0 && <span className="text-xs text-faint">{t.belt.chordHint}</span>}
          {queue.keys.map((k, i) => (
            <button key={i} type="button" className="h-7 shrink-0 rounded-md bg-surface-2 px-2 font-mono text-xs" onClick={() => setQueue((q) => queueRemoveAt(q, i))}>
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
                variant={armed === 'queue' ? 'danger' : 'primary'}
                icon={<Send className="size-3.5" />}
                onClick={async () => {
                  if (queueDanger && armed !== 'queue') {
                    arm('queue');
                    app.haptic('warning');
                    return;
                  }
                  arm(null);
                  const list = queue.keys;
                  setQueue({ keys: [] });
                  await sendKeys(list);
                }}
              >
                {armed === 'queue' ? t.belt.tapAgainKey : t.belt.sendKeys}
              </Button>
            </>
          )}
        </div>
      )}
      <div className="space-y-1.5" role="group" aria-label={t.belt.keyPad}>
        {rows.map((row, i) => (
          <div key={i} className="grid grid-cols-6 gap-1.5">
            {row.map((p) => {
              const id = fKeys ? `f:${p.index}` : padId(p.key, p.index);
              return (
                <PadKeyButton
                  key={id}
                  placed={p}
                  h={h}
                  armed={armed === id}
                  repeat={isRepeatable(p.key) && !chord}
                  send={() => hit(p.key, id)}
                  onTap={() => void hit(p.key, id)}
                />
              );
            })}
          </div>
        ))}
      </div>
      <div className={cx('flex gap-1.5', left && 'flex-row-reverse')}>
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
            if (c) {
              const base = c === ' ' ? 'space' : c;
              void hit({ steps: [base] }, `char:${base}`);
            }
          }}
          className={cx('w-12 rounded-lg border border-border bg-bg text-center font-mono text-sm', h)}
        />
        <KeyButton h={h} active={chord} onClick={() => setChord(!chord)}>
          {t.belt.chord}
        </KeyButton>
        <KeyButton h={h} active={fKeys} onClick={() => setFKeys(!fKeys)}>
          <span className="text-xs">{fKeys ? t.belt.mainKeys : t.belt.fKeys}</span>
        </KeyButton>
      </div>
      <div className={cx('flex min-h-9 items-center gap-2 text-xs text-muted', left && 'flex-row-reverse')}>
        <button type="button" aria-label={t.belt.editKeys} title={t.belt.editKeys} onClick={() => setEditing(true)} className="inline-flex size-9 shrink-0 items-center justify-center rounded-md active:bg-surface-2 pointer-coarse:size-11">
          <Pencil className="size-3.5" />
        </button>
        <div className="min-w-0 flex-1 text-center">
          {armed && armed !== 'queue' ? (
            <span className="text-danger">{t.belt.tapAgainKey}</span>
          ) : (
            echo && (
              <span className="inline-flex items-center gap-1 font-mono">
                {echo.label} {echo.ok === true && <Check className="size-3.5 text-ok" />}
                {echo.ok === false && <span className="text-danger">✕</span>}
              </span>
            )
          )}
        </div>
        <span className="size-9 shrink-0 pointer-coarse:size-11" aria-hidden />
      </div>
      {editing && <KeysEditor open onClose={() => setEditing(false)} />}
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
              'inline-flex h-9 items-center gap-1 rounded-full border border-border bg-bg px-3 text-sm',
              tapped && tapped !== r && 'opacity-40',
            )}
          >
            {tapped === r && <Check className="size-3.5 text-ok" />}
            {r}
          </button>
        ))}
      </div>
      <div className="py-1.5 text-2xs text-faint">{t.belt.editQuick}</div>
    </div>
  );
}

function AgentPanel({ actions, harness, onInsert }: { actions: PaneActions; harness: string | null; onInsert(text: string): void }) {
  const app = useApp();
  const cmds = slashCommandsFor(harness);
  const [armed, setArmed] = useState<string | null>(null);
  const [sent, setSent] = useState<string | null>(null);
  if (!cmds.length) return <div className="px-3 py-3 text-sm text-muted">{t.belt.noCommands}</div>;
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
          <span className={cx('font-mono text-sm', c.dangerous ? 'text-danger' : 'text-accent')}>{c.command}</span>
          <span className="min-w-0 flex-1 truncate text-xs text-muted">{armed === c.command ? t.composer.tapAgain : c.description}</span>
          {sent === c.command && <Check className="size-3.5 text-ok" />}
        </button>
      ))}
    </div>
  );
}

function DisplayPanel({ zen, setZen }: { zen?: boolean; setZen?(v: boolean): void }) {
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
        <span className="text-sm">{t.belt.leftHand}</span>
        <Toggle label={t.belt.leftHand} checked={prefs.leftHand} onChange={(v) => app.prefs.patch({ leftHand: v })} />
      </div>
      {setZen && (
        <div className="flex items-center justify-between">
          <span className="text-sm">{t.pane.zen}</span>
          <Toggle label={t.pane.zen} checked={!!zen} onChange={setZen} />
        </div>
      )}
    </div>
  );
}
