// Composer (spec 16 §9.1): a rounded box — the text on top, a row of controls below (attach,
// the agent's harness · model and permission mode as read-only labels, keys/quick replies in a
// ⋯ popover, voice, send or stop). Keeps clear / undo clear, the "You sent:" preview, the
// destructive second-tap guard, attachments (#N chips) and voice (consent first; a transcript
// is never auto-sent).

import { useEffect, useMemo, useRef, useState, type ClipboardEvent, type Dispatch, type ReactNode, type SetStateAction } from 'react';
import { ArrowUp, Camera, Loader2, Mic, MoreHorizontal, Paperclip, Plus, RotateCcw, ShieldCheck, ShieldOff, Square, X } from 'lucide-react';
import type { AgentCommand, AgentRun } from '@vibeke/core';
import { useApp, usePrefs } from '../../app/hooks';
import { Button, IconButton, Notice, Sheet, cx } from '../../components/ui';
import { MenuButton } from '../workspace/menu';
import { useMediaQuery } from '../../app/shell';
import { t } from '../../i18n';
import { errorMessage } from '../../lib/answer';
import { base64Std } from '../../lib/format';
import { composerShowsStop, destructiveReason } from '../../lib/guards';
import { CommandCache, commandTap, fallbackCommands, filterCommands, slashQuery } from '../../lib/pickers';
import type { PaneActions } from './actions';
import { ModelSwitcher } from './model-switcher';

/** Slash commands per host/run: loaded once, falling back to the built-in palette. */
const commandCache = new CommandCache();

interface Attachment {
  n: number;
  name: string;
  path: string | null;
  error: string | null;
}

const MAX_ATTACHMENT = 8 * 1024 * 1024;

export function Composer({
  hostId,
  actions,
  text,
  setText,
  isAgent,
  sttAvailable,
  run = null,
  interactions = null,
  more,
  onSent,
  placeholder,
  locked = false,
}: {
  hostId: string;
  actions: PaneActions;
  text: string;
  setText: Dispatch<SetStateAction<string>>;
  isAgent: boolean;
  sttAvailable: boolean;
  /** The agent (read-only `harness · model` and permission mode labels). */
  run?: AgentRun | null;
  /** The host's interactions (an open one on the run means it waits on the user, not working). */
  interactions?: readonly { run: string; status: string }[] | null;
  /** Content of the ⋯ popover (keys, quick replies, slash commands). */
  more?: ReactNode;
  onSent?: () => void;
  /** Overrides the agent / shell placeholder (e.g. typing into an agent's own terminal). */
  placeholder?: string;
  /** A picker or unknown dialog is open on the agent: typing is paused until it is answered. */
  locked?: boolean;
}) {
  const [moreOpen, setMoreOpen] = useState(false);
  const roomy = useMediaQuery('(min-width: 640px)');
  const app = useApp();
  const prefs = usePrefs();
  const [cleared, setCleared] = useState<string | null>(null);
  const [lastSent, setLastSent] = useState<string | null>(null);
  const [armed, setArmed] = useState<string | null>(null);
  const [sending, setSending] = useState(false);
  const [atts, setAtts] = useState<Attachment[]>([]);
  const [voice, setVoice] = useState<'idle' | 'consent' | 'listening' | 'recording' | 'transcribing'>('idle');
  const stopRef = useRef<(() => Promise<void>) | null>(null);
  const cancelRef = useRef<(() => void) | null>(null);
  const fileRef = useRef<HTMLInputElement>(null);
  const photoRef = useRef<HTMLInputElement>(null);
  const taRef = useRef<HTMLTextAreaElement>(null);
  const nextN = useRef(1);

  useEffect(() => setArmed(null), [text]);
  useEffect(() => {
    const ta = taRef.current;
    if (!ta) return;
    ta.style.height = 'auto';
    ta.style.height = `${Math.min(ta.scrollHeight, 160)}px`;
  }, [text]);
  useEffect(() => () => cancelRef.current?.(), []);

  // ---- slash commands ----
  const [cmds, setCmds] = useState<AgentCommand[]>([]);
  const [active, setActive] = useState(0);
  const [slashArmed, setSlashArmed] = useState<string | null>(null);
  const [dismissed, setDismissed] = useState<string | null>(null);
  const cmdKey = run ? `${hostId}/${run.id}` : null;
  const wantsSlash = isAgent && !locked && !!run && slashQuery(text) !== null;
  useEffect(() => {
    if (!wantsSlash || !run || !cmdKey) return;
    const cached = commandCache.peek(cmdKey);
    if (cached) return setCmds(cached);
    setCmds(fallbackCommands(run.harness));
    let live = true;
    void commandCache.load(cmdKey, app.conn(hostId), run.id, run.harness).then((l) => live && setCmds(l));
    return () => {
      live = false;
    };
  }, [wantsSlash, cmdKey]);
  const matches = useMemo(() => (wantsSlash && dismissed !== text ? filterCommands(cmds, text) : []), [wantsSlash, cmds, text, dismissed]);
  useEffect(() => {
    setActive(0);
    setSlashArmed(null);
  }, [text]);

  const pickCommand = (c: AgentCommand) => {
    const r = commandTap(c, slashArmed);
    app.haptic(r.do === 'arm' ? 'warning' : 'tap');
    if (r.do === 'insert') {
      setText(r.text);
      taRef.current?.focus();
    } else if (r.do === 'arm') setSlashArmed(c.name);
    else void send(r.text);
  };

  const send = async (override?: string) => {
    if (locked) return;
    const body = (override ?? text).trim();
    if (!body || sending) return;
    const why = destructiveReason(body);
    if (why && armed !== body) {
      setArmed(body);
      app.haptic('warning');
      return;
    }
    setSending(true);
    setArmed(null);
    app.haptic('tap');
    const ok = await actions.text(body);
    setSending(false);
    if (ok) {
      onSent?.();
      setLastSent(body);
      setText('');
      setAtts([]);
      nextN.current = 1;
      setCleared(null);
    }
  };

  const clear = () => {
    if (!text) return;
    setCleared(text);
    setText('');
    setAtts([]);
  };

  const upload = async (file: File) => {
    const n = nextN.current++;
    setAtts((a) => [...a, { n, name: file.name || `paste-${n}`, path: null, error: null }]);
    try {
      if (file.size > MAX_ATTACHMENT) throw new Error('> 8 MiB');
      const data = new Uint8Array(await file.arrayBuffer());
      const conn = app.conn(hostId);
      if (!conn) throw new Error(t.conn.hostOffline);
      const r = await conn.request('attachment.put', { name: file.name || `paste-${n}.png`, mime: file.type || 'application/octet-stream', data_b64: base64Std(data) }, { timeoutMs: 120_000 });
      setAtts((a) => a.map((x) => (x.n === n ? { ...x, path: r.path } : x)));
      setText((cur) => `${cur}${cur && !cur.endsWith(' ') ? ' ' : ''}${r.path} `);
    } catch (e) {
      setAtts((a) => a.map((x) => (x.n === n ? { ...x, error: errorMessage(e) } : x)));
    }
  };

  const removeAtt = (a: Attachment) => {
    setAtts((l) => l.filter((x) => x.n !== a.n));
    if (a.path) setText((cur) => cur.replace(`${a.path} `, '').replace(a.path!, ''));
  };

  const onPaste = (e: ClipboardEvent) => {
    const files = [...e.clipboardData.files];
    if (files.length) {
      e.preventDefault();
      files.forEach((f) => void upload(f));
    }
  };

  // ---- voice ----
  const speech = app.platform.speech;
  const canBrowser = !!speech?.recognizer && !!speech.recognize;
  const canHost = !!speech?.recorder && !!speech.record && sttAvailable;
  const voiceAvailable = canBrowser || canHost;
  const append = (s: string) => {
    if (s.trim()) setText((cur) => `${cur}${cur && !/\s$/.test(cur) ? ' ' : ''}${s.trim()}`);
  };

  const startVoice = async (mode: 'browser' | 'host') => {
    if (mode === 'browser' && speech?.recognize) {
      const r = speech.recognize(() => {});
      setVoice('listening');
      cancelRef.current = r.cancel;
      stopRef.current = async () => {
        const final = await r.stop().catch(() => '');
        append(final);
        setVoice('idle');
      };
    } else if (speech?.record) {
      try {
        const rec = await speech.record();
        setVoice('recording');
        cancelRef.current = rec.cancel;
        stopRef.current = async () => {
          setVoice('transcribing');
          try {
            const audio = await rec.stop();
            const r = await app.conn(hostId)!.request('stt.transcribe', { mime: audio.mime, data_b64: base64Std(audio.data) }, { timeoutMs: 90_000 });
            append(r.text);
          } catch (e) {
            app.toast(errorMessage(e), 'error');
          }
          setVoice('idle');
        };
      } catch (e) {
        app.toast(errorMessage(e), 'error');
        setVoice('idle');
      }
    }
  };

  const voiceTap = () => {
    if (voice === 'listening' || voice === 'recording') {
      void stopRef.current?.();
      return;
    }
    if (!voiceAvailable) return app.toast(t.composer.noVoice);
    if (canBrowser && prefs.speechConsent === null) return setVoice('consent');
    if (canBrowser && prefs.speechConsent) return void startVoice('browser');
    if (canHost) return void startVoice('host');
    setVoice('consent');
  };

  const why = armed ? destructiveReason(armed) : null;

  const modeLabel = run?.permission_mode ? permissionLabel(run.permission_mode) : run?.yolo ? permissionLabel('bypassPermissions') : null;
  const open = run?.yolo || run?.permission_mode === 'bypassPermissions';
  const canStop = composerShowsStop(run, interactions, text);

  return (
    <div className="px-3 pb-3 pt-1 sm:px-4">
      <div className="mx-auto w-full max-w-[780px]">
        {lastSent && !text && (
          <div className="mb-1 flex items-center gap-2 px-1 text-xs text-muted">
            <span className="shrink-0">{t.composer.youSent}</span>
            <span className="min-w-0 flex-1 truncate font-mono">{lastSent}</span>
            <button type="button" aria-label={t.close} onClick={() => setLastSent(null)}>
              <X className="size-3.5" />
            </button>
          </div>
        )}
        {locked && <Notice className="mb-1.5">{t.picker.lockedHint}</Notice>}
        {matches.length > 0 && (
          <div id="slash-list" role="listbox" aria-label={t.slash.label} className="mb-1.5 max-h-56 overflow-y-auto rounded-xl border border-border bg-surface py-1">
            {matches.map((c, i) => (
              <button
                key={c.name}
                id={`slash-${i}`}
                type="button"
                role="option"
                aria-selected={i === active}
                onMouseDown={(e) => e.preventDefault()}
                onClick={() => pickCommand(c)}
                className={cx('flex w-full items-center gap-2 px-3 py-1.5 text-left pointer-coarse:py-2.5', i === active ? 'bg-surface-2' : 'active:bg-surface-2')}
              >
                <span className={cx('font-mono text-sm', c.dangerous ? 'text-danger' : 'text-accent')}>{c.name}</span>
                <span className="min-w-0 flex-1 truncate text-xs text-muted">{slashArmed === c.name ? t.slash.tapAgain : c.description}</span>
                {c.opens_picker && <span className="shrink-0 text-2xs text-faint">{t.slash.opensPicker}</span>}
              </button>
            ))}
          </div>
        )}
        {why && <Notice tone="danger" className="mb-1.5">{t.composer.reallySend(why)} — {t.composer.tapAgain}</Notice>}
        {moreOpen && more && (
          <div className="mb-1.5 overflow-hidden rounded-xl border border-border bg-surface" role="region" aria-label={t.composer2.more}>
            {more}
          </div>
        )}
        <div
          className={cx(
            'rounded-2xl border bg-surface transition-colors focus-within:border-border-strong',
            armed ? 'border-del/60' : 'border-border',
          )}
        >
          {atts.length > 0 && (
            <div className="flex flex-wrap gap-1.5 px-3 pt-2.5">
              {atts.map((a) => (
                <span key={a.n} className={cx('inline-flex h-6 items-center gap-1 rounded-full border px-2 text-xs', a.error ? 'border-danger text-danger' : 'border-border')}>
                  <span className="font-semibold">#{a.n}</span>
                  <span className="max-w-32 truncate">{a.error ? `${t.composer.uploadFailed}: ${a.error}` : a.name}</span>
                  {!a.path && !a.error && <Loader2 className="size-3 animate-spin" />}
                  <button type="button" aria-label={t.remove} onClick={() => removeAtt(a)}>
                    <X className="size-3" />
                  </button>
                </span>
              ))}
            </div>
          )}
          {(voice === 'listening' || voice === 'recording' || voice === 'transcribing') && (
            <div className="flex items-center gap-2 px-3 pt-2.5 text-sm text-muted">
              <span className="size-2 animate-pulse rounded-full bg-danger" />
              {voice === 'listening' ? t.composer.listening : voice === 'recording' ? t.composer.recording : t.composer.transcribing}
            </div>
          )}
          <textarea
            ref={taRef}
            value={text}
            rows={1}
            disabled={locked}
            role={matches.length > 0 ? 'combobox' : undefined}
            aria-expanded={matches.length > 0 ? true : undefined}
            aria-controls={matches.length > 0 ? 'slash-list' : undefined}
            aria-activedescendant={matches.length > 0 ? `slash-${Math.min(active, matches.length - 1)}` : undefined}
            aria-label={placeholder ?? (isAgent ? t.composer2.placeholder : t.composer2.placeholderShell)}
            onChange={(e) => setText(e.target.value)}
            onPaste={onPaste}
            onKeyDown={(e) => {
              if (matches.length > 0) {
                if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
                  e.preventDefault();
                  setActive((a) => (a + (e.key === 'ArrowDown' ? 1 : -1) + matches.length) % matches.length);
                  return;
                }
                if ((e.key === 'Enter' && !e.metaKey && !e.ctrlKey && !e.shiftKey) || e.key === 'Tab') {
                  e.preventDefault();
                  pickCommand(matches[Math.min(active, matches.length - 1)]!);
                  return;
                }
                if (e.key === 'Escape') {
                  e.preventDefault();
                  setDismissed(text);
                  return;
                }
              }
              if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
                e.preventDefault();
                void send();
              }
            }}
            placeholder={placeholder ?? (isAgent ? (roomy ? t.composer2.placeholder : t.composer2.placeholderShort) : t.composer2.placeholderShell)}
            autoCapitalize="off"
            autoCorrect="off"
            spellCheck={false}
            className="block min-h-11 w-full resize-none bg-transparent px-3.5 pb-1 pt-3 text-[16px] leading-snug text-fg outline-none placeholder:text-faint sm:text-[14px]"
          />
          <div className="flex items-center gap-1 px-2 pb-2">
            <MenuButton
              label={t.composer2.attach}
              icon={<Plus />}
              align="left"
              placement="up"
              items={[
                { label: t.composer.file, icon: <Paperclip />, onSelect: () => fileRef.current?.click() },
                { label: t.composer.photo, icon: <Camera />, onSelect: () => photoRef.current?.click() },
              ]}
            />
            {run && <ModelSwitcher hostId={hostId} run={run} actions={actions} disabled={locked || !isAgent || !!run.ended_at_ms} />}
            {modeLabel && (
              <span className={cx('hidden h-7 shrink-0 items-center gap-1 rounded-md px-1.5 text-xs sm:inline-flex', open ? 'text-need' : 'text-muted')} title={t.composer2.mode}>
                {open ? <ShieldOff className="size-3.5" /> : <ShieldCheck className="size-3.5" />}
                {modeLabel}
              </span>
            )}
            <span className="flex-1" />
            {more && (
              <IconButton label={t.composer2.more} aria-expanded={moreOpen} active={moreOpen} onClick={() => setMoreOpen(!moreOpen)}>
                <MoreHorizontal />
              </IconButton>
            )}
            {text ? (
              <IconButton label={t.composer.clear} onClick={clear}>
                <X />
              </IconButton>
            ) : cleared ? (
              <IconButton
                label={t.composer.undoClear}
                onClick={() => {
                  setText(cleared);
                  setCleared(null);
                }}
              >
                <RotateCcw />
              </IconButton>
            ) : (
              <IconButton label={t.composer.voice} onClick={voiceTap} active={voice !== 'idle'} disabled={!voiceAvailable}>
                {voice === 'listening' || voice === 'recording' ? <Square className="text-danger" /> : <Mic />}
              </IconButton>
            )}
            {canStop ? (
              <button
                type="button"
                aria-label={t.pane.interrupt}
                title={t.pane.interrupt}
                onClick={() => void actions.interrupt()}
                className="vk-focus inline-flex size-7 shrink-0 items-center justify-center rounded-full bg-fg text-bg pointer-coarse:size-9"
              >
                <Square className="size-3 fill-current" />
              </button>
            ) : (
              <button
                type="button"
                aria-label={t.send}
                title={t.send}
                disabled={!text.trim() || sending || locked}
                onClick={() => void send()}
                className={cx(
                  'vk-focus inline-flex size-7 shrink-0 items-center justify-center rounded-full disabled:bg-surface-3 disabled:text-faint pointer-coarse:size-9',
                  armed ? 'bg-danger text-danger-fg' : 'bg-fg text-bg',
                )}
              >
                {sending ? <Loader2 className="size-4 animate-spin" /> : <ArrowUp className="size-4" strokeWidth={2.25} />}
              </button>
            )}
          </div>
        </div>
      </div>
      <input ref={fileRef} type="file" multiple hidden onChange={(e) => [...(e.target.files ?? [])].forEach((f) => void upload(f))} />
      <input ref={photoRef} type="file" accept="image/*" multiple hidden onChange={(e) => [...(e.target.files ?? [])].forEach((f) => void upload(f))} />
      <Sheet open={voice === 'consent'} onClose={() => setVoice('idle')} title={t.composer.voiceConsentTitle}>
        <div className="space-y-3">
          <p className="text-sm text-muted">{t.composer.voiceConsent}</p>
          {canBrowser && (
            <Button
              block
              variant="primary"
              onClick={() => {
                app.prefs.patch({ speechConsent: true });
                void startVoice('browser');
              }}
            >
              {t.composer.useBrowser}
            </Button>
          )}
          {canHost && (
            <Button
              block
              variant="outline"
              onClick={() => {
                app.prefs.patch({ speechConsent: false });
                void startVoice('host');
              }}
            >
              {t.composer.useHost}
            </Button>
          )}
          {!canHost && !canBrowser && <Notice>{t.composer.noVoice}</Notice>}
        </div>
      </Sheet>
    </div>
  );
}

/** Read-only label for the agent's permission mode (Claude's names; others shown as given). */
export function permissionLabel(mode: string): string {
  const known: Record<string, string> = {
    bypassPermissions: 'Full access',
    acceptEdits: 'Accept edits',
    plan: 'Plan mode',
    default: 'Ask first',
    'full-access': 'Full access',
    'read-only': 'Read only',
    auto: 'Auto',
  };
  return known[mode] ?? mode;
}
