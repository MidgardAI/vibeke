// The design review screen (spec 20 §3, §7.2): `#/review?d=…`. It pairs like `#/pair` (with a
// GitHub sign-in when the link asks for one), then shows the preview: in this browser through
// the content origin (lib/review-controller.ts), or as the host's screencast (§3.7). A slim
// toolbar (Annotate, Reload, Notes, Use host browser) and the notes sheet sit on top. It skips
// the host list and the dashboard: a review connection only says hello, reads `review.info` and
// polls its notes.

import { useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore, type ReactNode } from 'react';
import { ExternalLink, Globe, LogIn, MessageSquarePlus, MonitorPlay, NotebookPen, RotateCw, ShieldCheck } from 'lucide-react';
import {
  CookieJar,
  PairingError,
  isReviewLink,
  needsAttestation,
  parseLink,
  previewAuthority,
  previewPath,
  reviewContentOrigin,
  type AnnotationDraft,
  type AttestResult,
  type HostConnectionApi,
  type PairingLink,
  type PickPayload,
  type ReviewConnection,
  type ReviewRecord,
} from '@vibeke/core';
import { ConnOverrideContext, useApp } from '../app/hooks';
import { AnnotationNotes } from '../components/annotation-notes';
import { PhonePreview } from '../components/phone-preview';
import { Button, Card, IconButton, Notice, Pill, Sheet, Spinner } from '../components/ui';
import { t } from '../i18n';
import { errorMessage } from '../lib/answer';
import { startAttestation, takeAttestation } from '../lib/attest-flow';
import { noteTitle } from '../lib/annotations';
import { ReviewController } from '../lib/review-controller';
import { pairingErrorMessage } from './pair';

/** One read of the attestation result per invitation, also under React's double effects. */
const attestations = new Map<string, AttestResult | null>();
function attestationFor(app: ReturnType<typeof useApp>, pid: string): AttestResult | null {
  if (!attestations.has(pid)) attestations.set(pid, takeAttestation(app, pid));
  return attestations.get(pid) ?? null;
}

type Join =
  | { k: 'loading' }
  | { k: 'form'; error?: string; allowed?: string[] }
  | { k: 'busy'; what: 'signin' | 'pair' }
  | { k: 'ready'; record: ReviewRecord };

export function ReviewScreen({ d }: { d: string | null }) {
  const app = useApp();
  const link = useMemo((): PairingLink | null => {
    if (!d) return null;
    try {
      const l = parseLink(d);
      return isReviewLink(l) ? l : null;
    } catch {
      return null;
    }
  }, [d]);
  const [join, setJoin] = useState<Join>({ k: 'loading' });

  const pairNow = useCallback(
    async (l: PairingLink, attest?: string) => {
      setJoin({ k: 'busy', what: 'pair' });
      try {
        const rec = (await app.pair(l, app.prefs.get().deviceName || app.platform.defaultDeviceName, () => {}, attest)) as ReviewRecord;
        app.haptic('success');
        setJoin({ k: 'ready', record: rec });
      } catch (e) {
        app.haptic('error');
        const refusal = e instanceof PairingError ? e.refusal : undefined;
        const message = (refusal && t.pair.refusals[refusal]) || pairingErrorMessage(e);
        setJoin({ k: 'form', error: message, allowed: e instanceof PairingError ? e.allowed : undefined });
      }
    },
    [app],
  );

  // A stored review goes straight to the screen; a sign-in that just came back pairs at once.
  useEffect(() => {
    if (!link || !app.platform.review) return;
    let live = true;
    void (async () => {
      const rec = await app.findReview(link.host, link.pid);
      if (!live) return;
      if (rec) return setJoin({ k: 'ready', record: rec });
      const a = needsAttestation(link) ? attestationFor(app, link.pid) : null;
      if (a?.ok) {
        attestations.delete(link.pid);
        return void pairNow(link, a.token);
      }
      setJoin({ k: 'form', error: a && !a.ok ? t.pair.github.failed(a.error) : undefined });
    })();
    return () => {
      live = false;
    };
  }, [app, link, pairNow]);

  if (!link) {
    return (
      <Centered>
        <Notice tone="danger">{t.pair.errors.invalid}</Notice>
      </Centered>
    );
  }
  if (!app.platform.review) {
    // The desktop app keeps one implementation of this screen: the browser app's.
    const url = `${(app.platform.browserAppUrl ?? 'https://app.vibeke.dev').replace(/\/+$/, '')}/#/review?d=${d}`;
    return (
      <Centered>
        <Notice>{t.review.unsupported}</Notice>
        <Button variant="primary" icon={<ExternalLink />} onClick={() => app.platform.openExternal(url)}>
          {t.pair.github.openInBrowser}
        </Button>
      </Centered>
    );
  }
  if (join.k === 'ready') return <Reviewer record={join.record} />;
  if (join.k === 'loading') {
    return (
      <Centered>
        <Spinner />
      </Centered>
    );
  }

  const s = link.share!;
  const github = needsAttestation(link);
  const expired = app.platform.clock.now() / 1000 >= link.exp;
  return (
    <Centered>
      <Card className="w-full max-w-md space-y-4 p-5">
        <div className="space-y-1">
          <div className="text-lg font-semibold">{t.review.from(link.name, s.preview?.label || s.label || t.review.thePreview)}</div>
          <div className="text-sm text-muted">{s.notes === 'held' ? t.review.notesHeld : t.review.notesDirect}</div>
          {github && <div className="text-sm text-muted">{t.pair.github.needed}</div>}
        </div>
        {expired && <Notice tone="danger">{t.pair.errors.expired}</Notice>}
        {join.k === 'form' && join.error && <Notice tone="danger">{join.error}</Notice>}
        {join.k === 'form' && join.allowed && join.allowed.length > 0 && <Notice>{t.pair.github.linkFor(join.allowed)}</Notice>}
        {join.k === 'busy' ? (
          <div className="flex items-center gap-3 text-sm">
            <Spinner />
            <span>{join.what === 'signin' ? t.pair.github.signingIn : t.review.opening}</span>
          </div>
        ) : github ? (
          <Button
            variant="primary"
            size="lg"
            block
            disabled={expired}
            icon={<LogIn className="size-5" />}
            onClick={() => {
              setJoin({ k: 'busy', what: 'signin' });
              startAttestation(app, link, d!, 'review').catch((e) => setJoin({ k: 'form', error: errorMessage(e) }));
            }}
          >
            {join.k === 'form' && (join.error || join.allowed) ? t.pair.github.signInAgain : t.pair.github.signIn}
          </Button>
        ) : (
          <Button variant="primary" size="lg" block disabled={expired} icon={<ShieldCheck className="size-5" />} onClick={() => void pairNow(link)}>
            {t.review.open}
          </Button>
        )}
      </Card>
    </Centered>
  );
}

function Centered({ children }: { children: ReactNode }) {
  return <div className="flex h-full flex-col items-center justify-center gap-4 p-4 pt-safe">{children}</div>;
}

const supportsContentOrigin = (): boolean => typeof navigator !== 'undefined' && 'serviceWorker' in navigator && typeof MessageChannel !== 'undefined';

function Reviewer({ record }: { record: ReviewRecord }) {
  const app = useApp();
  const conn: ReviewConnection = useMemo(() => app.reviewConnection(record), [app, record]);
  const state = useSyncExternalStore(conn.subscribe, conn.getSnapshot);
  const info = state.review;
  const contentOrigin = useMemo(
    () => (typeof window === 'undefined' ? null : reviewContentOrigin(window.location.origin, record.sid, app.platform.review?.contentOriginPattern)),
    [record.sid, app],
  );
  const [mode, setMode] = useState<'content' | 'screencast'>(contentOrigin && supportsContentOrigin() ? 'content' : 'screencast');
  const [annotating, setAnnotating] = useState(false);
  const [notesOpen, setNotesOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const controller = useRef<ReviewController | null>(null);
  const bridge = useRef<HTMLIFrameElement>(null);
  const content = useRef<HTMLIFrameElement>(null);
  const owner = info?.owner ?? '';
  const previewId = info?.preview.id ?? record.preview.id;

  const addPick = useCallback(
    async (p: PickPayload) => {
      try {
        await conn.request('browser.annotation_add', {
          target: { preview: previewId },
          kind: p.kind,
          selector: p.selector,
          role: p.role,
          name: p.name,
          text: p.text,
          html: p.html,
          rect: p.rect,
          covers: p.covers,
          url: p.url,
          viewport: p.viewport,
          capture: true,
        });
        app.toast(t.review.added, 'ok', 1500);
        await conn.refresh();
      } catch (e) {
        app.toast(errorMessage(e), 'error');
      }
    },
    [app, conn, previewId],
  );

  // The content origin: one controller per review screen, kept across reconnects.
  const authority = info ? previewAuthority(info) : null;
  const firstPath = info ? previewPath(info) : '/';
  // `info` arrives with the first connection and stays across reconnects, like the controller.
  const canBoot = mode === 'content' && !!contentOrigin && !!authority;
  useEffect(() => {
    if (!canBoot || controller.current || !bridge.current || !content.current) return;
    const store = app.platform.review!.cookies;
    let jar = new CookieJar();
    let saveTimer: ReturnType<typeof setTimeout> | null = null;
    let disposed = false;
    const c = new ReviewController({
      contentOrigin: contentOrigin!,
      preview: previewId,
      authority: authority!,
      path: firstPath,
      link: () => conn.streamLink(),
      get jar() {
        return jar;
      },
      jarChanged: () => {
        if (saveTimer) return;
        saveTimer = setTimeout(() => {
          saveTimer = null;
          void store.set(record.sid, jar.toJSON(Date.now())).catch(() => {});
        }, 500);
      },
      now: () => Date.now(),
      random: (n) => app.platform.random(n),
      onPick: (p) => void addPick(p),
      onState: (s) => {
        if (s.phase === 'failed' && !disposed) {
          app.toast(t.review.contentFailed, 'warn', 5000);
          setMode('screencast');
        }
      },
    });
    controller.current = c;
    // The jar first (requests need its cookies), then the frames.
    void store
      .get(record.sid)
      .catch(() => null)
      .then((data) => {
        if (disposed) return;
        jar = new CookieJar(data);
        c.attach(bridge.current!, content.current!);
      });
    return () => {
      disposed = true;
      if (saveTimer) {
        clearTimeout(saveTimer);
        void store.set(record.sid, jar.toJSON(Date.now())).catch(() => {});
      }
      c.dispose();
      controller.current = null;
    };
    // Boot once per mode; a reconnect keeps the controller (it reads the live link per request).
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [canBoot && mode === 'content']);

  useEffect(() => {
    controller.current?.setAnnotate(annotating);
  }, [annotating]);

  const override = useCallback((id: string): HostConnectionApi | undefined => (id === record.host_id ? conn : undefined), [conn, record.host_id]);

  const draft = state.draft;
  const count = draft?.notes.filter((n) => n.kind !== 'page').length ?? 0;
  const run = async (f: () => Promise<unknown>) => {
    setBusy(true);
    try {
      await f();
      await conn.refresh();
    } catch (e) {
      app.toast(errorMessage(e), 'error');
    } finally {
      setBusy(false);
    }
  };
  const send = () =>
    run(async () => {
      if (!draft) return;
      await conn.request('browser.annotations_send', { draft: draft.id });
      await conn.rememberSent({ ...draft, state: info?.notes === 'held' ? 'held' : 'queued' });
      app.toast(info?.notes === 'held' ? (t.review.sentState.held as (o: string) => string)(owner) : t.annotate.sent.queued, 'info');
      setNotesOpen(false);
    });
  const copy = async () => {
    if (!draft) return;
    const text = [...draft.notes.filter((n) => n.kind !== 'page').map((n) => `${n.n}. ${noteTitle(n)}: ${n.note}`), draft.page_note ?? ''].filter(Boolean).join('\n');
    await app.platform.clipboard.writeText(text).catch(() => {});
    app.toast(t.annotate.copied, 'info');
  };

  if (state.status === 'revoked' || state.status === 'ticket_expired' || state.status === 'expired') {
    return (
      <Centered>
        <Notice tone="danger">{state.status === 'expired' ? t.review.expired : t.review.revoked}</Notice>
        <Button variant="ghost" onClick={() => void app.forgetReview(record).then(() => window.location.reload())}>
          {t.review.forget}
        </Button>
      </Centered>
    );
  }

  return (
    <ConnOverrideContext.Provider value={override}>
      <div className="flex h-full min-h-0 flex-col pt-safe px-safe">
        <div role="toolbar" aria-label={t.review.toolbar} className="flex shrink-0 items-center gap-1 border-b border-border px-2 py-1">
          <span className="mr-auto inline-flex min-w-0 items-center gap-2 truncate px-1 text-sm font-medium">
            <Globe className="size-4 shrink-0 text-muted" aria-hidden />
            <span className="truncate">{info?.preview.label || record.preview.label || t.review.title}</span>
            {state.status !== 'online' && <Pill>{t.conn.reconnecting}</Pill>}
          </span>
          {mode === 'content' && (
            <>
              <IconButton label={annotating ? t.review.stopAnnotate : t.review.annotate} aria-pressed={annotating} active={annotating} onClick={() => setAnnotating((v) => !v)}>
                <MessageSquarePlus />
              </IconButton>
              <IconButton label={t.review.reload} onClick={() => controller.current?.reload()}>
                <RotateCw />
              </IconButton>
            </>
          )}
          <Button size="sm" variant="outline" icon={<NotebookPen />} onClick={() => setNotesOpen(true)}>
            {t.review.notes(count)}
          </Button>
          {contentOrigin && supportsContentOrigin() && (
            <Button size="sm" variant="ghost" icon={mode === 'content' ? <MonitorPlay /> : <Globe />} onClick={() => setMode((m) => (m === 'content' ? 'screencast' : 'content'))}>
              {mode === 'content' ? t.review.hostBrowser : t.review.ownBrowser}
            </Button>
          )}
        </div>
        {annotating && mode === 'content' && <div className="shrink-0 bg-surface px-3 py-1 text-xs text-muted">{t.review.annotateHint}</div>}
        <div className="relative flex min-h-0 flex-1 flex-col bg-white">
          {mode === 'content' ? (
            <>
              <iframe ref={bridge} title="" aria-hidden className="hidden" />
              <iframe
                ref={content}
                title={t.review.frameTitle}
                className="size-full flex-1 border-0"
                allow="clipboard-read; clipboard-write; fullscreen"
              />
              {!canBoot && (
                <div className="absolute inset-0 flex items-center justify-center bg-surface">
                  {info && !contentOrigin ? <Notice>{t.review.noContentOrigin}</Notice> : <Spinner />}
                </div>
              )}
            </>
          ) : state.status === 'online' ? (
            <PhonePreview hostId={record.host_id} preview={{ id: previewId }} confined />
          ) : (
            <div className="flex flex-1 items-center justify-center bg-surface">
              <Spinner />
            </div>
          )}
        </div>
        <Sheet open={notesOpen} onClose={() => setNotesOpen(false)} title={t.annotate.title}>
          <div className="space-y-4">
            <AnnotationNotes
              draft={draft}
              busy={busy}
              onEdit={(n, note) => void run(() => conn.request('browser.annotation_update', { draft: draft!.id, n, note }))}
              onPageNote={(note) => void run(() => conn.request('browser.annotation_update', { draft: draft!.id, page_note: note }))}
              onRemove={(n) => void run(() => conn.request('browser.annotation_remove', { draft: draft!.id, n }))}
              onSend={() => void send()}
              onCopy={() => void copy()}
              onClear={() => void run(() => conn.request('browser.annotations_clear', { draft: draft!.id }))}
            />
            <SentNotes drafts={state.sent} owner={owner} />
          </div>
        </Sheet>
      </div>
    </ConnOverrideContext.Provider>
  );
}

/** Sent drafts with their state: held ("waiting for <owner>"), queued, delivered. */
function SentNotes({ drafts, owner }: { drafts: AnnotationDraft[]; owner: string }) {
  if (!drafts.length) return null;
  return (
    <section aria-label={t.review.sent} className="space-y-2">
      <h3 className="text-sm font-semibold">{t.review.sent}</h3>
      {drafts.map((d) => {
        const label = t.review.sentState[d.state];
        const text = typeof label === 'function' ? label(owner) : (label ?? d.state);
        const notes = d.notes.filter((n) => n.kind !== 'page');
        return (
          <div key={d.id} className="rounded-lg border border-border bg-surface p-2">
            <div className="flex items-center gap-2">
              <span className="min-w-0 flex-1 truncate text-sm">{notes.length ? notes.map(noteTitle).join(' · ') : (d.page_note ?? '')}</span>
              <Pill>{text}</Pill>
            </div>
            <div className="text-xs text-muted">{t.review.sentCount(notes.length + (d.page_note ? 1 : 0))}</div>
          </div>
        );
      })}
    </section>
  );
}
