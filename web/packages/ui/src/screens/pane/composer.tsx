// Composer (spec 16 §9.1): a rounded box — the text on top, a row of controls below (attach,
// the agent's harness · model and permission mode as read-only labels, keys/quick replies in a
// ⋯ popover, voice, send or stop). Keeps clear / undo clear, the "You sent:" preview, the
// destructive second-tap guard, attachments (#N chips) and voice (consent first; a transcript
// is never auto-sent).

import { useEffect, useRef, useState, type ClipboardEvent, type Dispatch, type ReactNode, type SetStateAction } from 'react';
import { ArrowUp, Camera, Loader2, Mic, MoreHorizontal, Paperclip, Plus, RotateCcw, ShieldCheck, ShieldOff, Square, X } from 'lucide-react';
import type { AgentRun } from '@vibeke/core';
import { useApp, usePrefs } from '../../app/hooks';
import { Button, HarnessIcon, IconButton, Notice, Sheet, cx } from '../../components/ui';
import { MenuButton } from '../workspace/menu';
import { t } from '../../i18n';
import { errorMessage } from '../../lib/answer';
import { base64Std } from '../../lib/format';
import { destructiveReason } from '../../lib/guards';
import { harnessLabel } from '../../lib/harness';
import type { PaneActions } from './actions';

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
  working = false,
  more,
  onSent,
}: {
  hostId: string;
  actions: PaneActions;
  text: string;
  setText: Dispatch<SetStateAction<string>>;
  isAgent: boolean;
  sttAvailable: boolean;
  /** The agent (read-only `harness · model` and permission mode labels). */
  run?: AgentRun | null;
  /** The agent is working: an empty composer offers Stop (interrupt). */
  working?: boolean;
  /** Content of the ⋯ popover (keys, quick replies, slash commands). */
  more?: ReactNode;
  onSent?: () => void;
}) {
  const [moreOpen, setMoreOpen] = useState(false);
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

  const send = async () => {
    const body = text.trim();
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
  const canStop = working && !text.trim() && !!run;

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
            aria-label={isAgent ? t.composer2.placeholder : t.composer2.placeholderShell}
            onChange={(e) => setText(e.target.value)}
            onPaste={onPaste}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
                e.preventDefault();
                void send();
              }
            }}
            placeholder={isAgent ? t.composer2.placeholder : t.composer2.placeholderShell}
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
            {run && (
              <span className="inline-flex h-7 min-w-0 items-center gap-1.5 rounded-md px-1.5 text-xs text-muted" title={t.composer2.model}>
                <HarnessIcon harness={run.harness} />
                <span className="truncate">
                  {harnessLabel(run.harness)}
                  {run.model && <span className="text-faint"> · {run.model}</span>}
                </span>
              </span>
            )}
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
                disabled={!text.trim() || sending}
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
