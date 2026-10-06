// Composer (spec 16 §9.1): text box with clear / undo clear, "You sent:" preview, destructive
// second-tap guard, attachments (#N chips), voice (consent first; transcript never auto-sent).

import { useEffect, useRef, useState, type ClipboardEvent, type Dispatch, type SetStateAction } from 'react';
import { Camera, Loader2, Mic, Paperclip, RotateCcw, Send, Square, X } from 'lucide-react';
import { useApp, usePrefs } from '../../app/hooks';
import { Button, IconButton, Notice, Sheet, cx } from '../../components/ui';
import { t } from '../../i18n';
import { errorMessage } from '../../lib/answer';
import { base64Std } from '../../lib/format';
import { destructiveReason } from '../../lib/guards';
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
}: {
  hostId: string;
  actions: PaneActions;
  text: string;
  setText: Dispatch<SetStateAction<string>>;
  isAgent: boolean;
  sttAvailable: boolean;
}) {
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

  return (
    <div className="border-t border-border bg-surface px-2 pb-1.5 pt-1.5">
      {lastSent && !text && (
        <div className="mb-1 flex items-center gap-2 px-1 text-[12px] text-muted">
          <span className="shrink-0">{t.composer.youSent}</span>
          <span className="min-w-0 flex-1 truncate font-mono">{lastSent}</span>
          <button type="button" aria-label={t.close} onClick={() => setLastSent(null)}>
            <X className="size-3.5" />
          </button>
        </div>
      )}
      {why && <Notice tone="danger" className="mb-1.5">{t.composer.reallySend(why)} — {t.composer.tapAgain}</Notice>}
      {atts.length > 0 && (
        <div className="mb-1.5 flex flex-wrap gap-1.5">
          {atts.map((a) => (
            <span key={a.n} className={cx('inline-flex h-7 items-center gap-1 rounded-full border px-2 text-[12px]', a.error ? 'border-danger text-danger' : 'border-border')}>
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
        <div className="mb-1.5 flex items-center gap-2 px-1 text-[13px] text-accent">
          <span className="size-2 animate-pulse rounded-full bg-danger" />
          {voice === 'listening' ? t.composer.listening : voice === 'recording' ? t.composer.recording : t.composer.transcribing}
        </div>
      )}
      <div className="flex items-end gap-1">
        <IconButton label={t.composer.photo} onClick={() => photoRef.current?.click()}>
          <Camera className="size-5 text-muted" />
        </IconButton>
        <IconButton label={t.composer.file} onClick={() => fileRef.current?.click()} className="-ml-1">
          <Paperclip className="size-5 text-muted" />
        </IconButton>
        <textarea
          ref={taRef}
          value={text}
          rows={1}
          onChange={(e) => setText(e.target.value)}
          onPaste={onPaste}
          onKeyDown={(e) => {
            if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
              e.preventDefault();
              void send();
            }
          }}
          placeholder={isAgent ? t.composer.placeholderAgent : t.composer.placeholderShell}
          autoCapitalize="off"
          autoCorrect="off"
          spellCheck={false}
          className="min-h-10 min-w-0 flex-1 resize-none rounded-2xl border border-border bg-bg px-3 py-2 text-[16px] leading-snug placeholder:text-faint focus:outline-2 focus:outline-accent"
        />
        {text ? (
          <IconButton label={t.composer.clear} onClick={clear}>
            <X className="size-5 text-muted" />
          </IconButton>
        ) : cleared ? (
          <IconButton
            label={t.composer.undoClear}
            onClick={() => {
              setText(cleared);
              setCleared(null);
            }}
          >
            <RotateCcw className="size-5 text-muted" />
          </IconButton>
        ) : (
          <IconButton label={t.composer.voice} onClick={voiceTap} active={voice !== 'idle'} disabled={!voiceAvailable}>
            {voice === 'listening' || voice === 'recording' ? <Square className="size-4.5 text-danger" /> : <Mic className="size-5 text-muted" />}
          </IconButton>
        )}
        <button
          type="button"
          aria-label={t.send}
          disabled={!text.trim() || sending}
          onClick={() => void send()}
          className={cx(
            'inline-flex size-10 shrink-0 items-center justify-center rounded-full border border-transparent disabled:opacity-40',
            armed ? 'bg-danger text-danger-fg' : 'bg-accent text-accent-fg',
          )}
        >
          {sending ? <Loader2 className="size-5 animate-spin" /> : <Send className="size-4.5" />}
        </button>
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
