// One open interaction as a card (spec 16 §9.2): approvals with Allow / Allow always / Deny and
// swipe for low/medium risk, questions with options and free text, plan reviews, and the delivery
// state after answering.

import { useMemo, useRef, useState, type PointerEvent as ReactPointerEvent, type ReactNode } from 'react';
import { AlertTriangle, Check, CircleHelp, ClipboardList, ExternalLink, FileText, Loader2, RefreshCw, ShieldAlert, X } from 'lucide-react';
import { displayName, interactionRisk, paneTitle, swipeAllowed, type Decision, type InboxItem, type Interaction } from '@vibeke/core';
import { useAnswers, useApp, useHost, useNow } from '../app/hooks';
import { t } from '../i18n';
import { deliveryView, needsPane, type DeliveryView } from '../lib/answer';
import { shortDuration } from '../lib/format';
import { harnessLabel } from '../lib/harness';
import { navigate } from '../router';
import { DiffView } from './diff';
import { Markdown } from './markdown';
import { Button, Card, Notice, RiskBadge, Sheet, cx } from './ui';

const SWIPE_AT = 96;

export function useCardContext(item: InboxItem) {
  const host = useHost(item.host_id);
  const d = host?.dashboard;
  const ws = d?.workspaces.find((w) => w.id === (item.pane?.workspace ?? ''));
  const scope = host?.info?.scope ?? host?.record.scope ?? 'view';
  const online = host?.status === 'online';
  return {
    host,
    hostName: host?.info?.host_name ?? host?.record.name ?? item.host_id,
    workspace: ws ? displayName(ws) : null,
    harness: item.interaction.harness ?? item.run?.harness ?? null,
    online,
    scope,
    canAnswer: online && scope !== 'view' && item.interaction.answerable,
    multiHost: false,
  };
}

export function CardHeader({ item, showHost }: { item: InboxItem; showHost: boolean }) {
  const ctx = useCardContext(item);
  const now = useNow(10_000);
  const it = item.interaction;
  const parts = [harnessLabel(ctx.harness), ctx.workspace, item.pane ? paneTitle(item.pane) : null, showHost ? ctx.hostName : null].filter(
    (x): x is string => !!x,
  );
  return (
    <div className="flex items-center gap-2 text-[12px] text-muted">
      <KindIcon kind={it.kind} />
      <span className="min-w-0 flex-1 truncate">{parts.join(' · ')}</span>
      <span className="shrink-0 tabular-nums">{t.inbox.waiting(shortDuration(now - it.opened_at_ms))}</span>
      {it.kind === 'approval' && <RiskBadge risk={interactionRisk(it)} />}
    </div>
  );
}

function KindIcon({ kind }: { kind: Interaction['kind'] }) {
  const c = 'size-3.5 shrink-0';
  if (kind === 'question') return <CircleHelp className={c} />;
  if (kind === 'plan_review') return <ClipboardList className={c} />;
  if (kind === 'notice') return <FileText className={c} />;
  return <ShieldAlert className={c} />;
}

function Collapsible({ children, max = 180 }: { children: ReactNode; max?: number }) {
  const [open, setOpen] = useState(false);
  return (
    <div>
      <div className={cx('relative', !open && 'overflow-hidden')} style={open ? undefined : { maxHeight: max }}>
        {children}
        {!open && <div className="pointer-events-none absolute inset-x-0 bottom-0 h-8 bg-gradient-to-t from-surface" />}
      </div>
      <button type="button" className="mt-1 text-[13px] text-accent" onClick={() => setOpen(!open)}>
        {open ? t.inbox.showLess : t.inbox.showMore}
      </button>
    </div>
  );
}

function ActionPreview({ it }: { it: Interaction }) {
  const a = it.action;
  const [diffOpen, setDiffOpen] = useState(false);
  if (!a) return it.body_md ? <Markdown text={it.body_md} className="text-sm" /> : null;
  return (
    <div className="space-y-2">
      {a.summary && a.summary !== it.title && <div className="text-sm text-muted">{a.summary}</div>}
      {a.command && (
        <pre className="term max-h-40 overflow-auto whitespace-pre-wrap break-all rounded-xl border border-border px-2.5 py-2 text-[12.5px]">
          {a.command}
        </pre>
      )}
      {a.paths.length > 0 && (
        <div className="space-y-0.5 font-mono text-[12px] text-muted">
          {a.paths.slice(0, 3).map((p) => (
            <div key={p} className="truncate">
              {p}
            </div>
          ))}
          {a.paths.length > 3 && <div>{t.inbox.paths(a.paths.length)}</div>}
        </div>
      )}
      {a.diff && (
        <div>
          <button type="button" className="text-[13px] text-accent" onClick={() => setDiffOpen(!diffOpen)}>
            {t.inbox.diff} {diffOpen ? '▾' : '▸'}
          </button>
          {diffOpen && (
            <div className="mt-1 max-h-72 overflow-auto">
              <DiffView diff={a.diff} path={a.paths[0] ?? ''} fontSize={11.5} />
            </div>
          )}
        </div>
      )}
      {a.risk_reasons.length > 0 && (a.risk === 'high' || a.risk === 'unknown' || a.risk === 'medium') && (
        <div className="flex items-start gap-1.5 text-[12px] text-muted">
          <AlertTriangle className="mt-0.5 size-3.5 shrink-0 text-warn" />
          <span>{a.risk_reasons.join(' · ')}</span>
        </div>
      )}
      {it.body_md && <Markdown text={it.body_md} className="text-sm" />}
    </div>
  );
}

export function DeliveryLine({ view, error, onOpenPane, onRefresh }: { view: DeliveryView; error?: string; onOpenPane(): void; onRefresh(): void }) {
  const map: Record<DeliveryView, { text: string; tone: 'muted' | 'ok' | 'warn' | 'danger'; spin?: boolean }> = {
    sending: { text: t.inbox.sending, tone: 'muted', spin: true },
    delivering: { text: t.inbox.delivering, tone: 'muted', spin: true },
    delivered: { text: t.inbox.delivered, tone: 'ok' },
    recorded: { text: t.inbox.recorded, tone: 'ok' },
    failed: { text: t.inbox.failed, tone: 'danger' },
    unknown: { text: t.inbox.unknown, tone: 'warn' },
    superseded: { text: t.inbox.superseded, tone: 'muted' },
    stale: { text: t.inbox.stale, tone: 'warn' },
    error: { text: error ?? t.unknownError, tone: 'danger' },
    answered_elsewhere: { text: t.inbox.answeredElsewhere, tone: 'muted' },
  };
  const m = map[view];
  const color = { muted: 'text-muted', ok: 'text-ok', warn: 'text-warn', danger: 'text-danger' }[m.tone];
  return (
    <div className={cx('flex min-h-9 items-center gap-2 text-[13px]', color)} role="status">
      {m.spin ? <Loader2 className="size-4 animate-spin" /> : m.tone === 'ok' ? <Check className="size-4" /> : null}
      <span className="flex-1">{m.text}</span>
      {needsPane(view) && (
        <Button size="sm" variant="outline" icon={<ExternalLink className="size-3.5" />} onClick={onOpenPane}>
          {t.inbox.openPane}
        </Button>
      )}
      {(view === 'stale' || view === 'error') && (
        <Button size="sm" variant="outline" icon={<RefreshCw className="size-3.5" />} onClick={onRefresh}>
          {t.refresh}
        </Button>
      )}
    </div>
  );
}

export function InteractionCard({
  item,
  showHost = false,
  preselect = null,
  leaving = false,
}: {
  item: InboxItem;
  showHost?: boolean;
  preselect?: 'allow' | 'deny' | null;
  leaving?: boolean;
}) {
  const app = useApp();
  useAnswers();
  const ctx = useCardContext(item);
  const it = item.interaction;
  const key = `${item.host_id}/${it.id}`;
  const local = app.answers.get(key);
  const view = leaving && !local ? 'answered_elsewhere' : deliveryView(local, it);
  const locked = view !== null && view !== 'stale' && view !== 'error';
  const disabled = !ctx.canAnswer || locked;
  const [confirm, setConfirm] = useState<Decision | null>(null);

  const send = (params: { decision?: Decision; choices?: Record<string, string[]>; text?: string }, label: string) => {
    app.haptic('tap');
    void app.answer(item.host_id, it, params, label);
  };
  const decide = (d: Decision) => {
    const risky = interactionRisk(it) === 'high' || interactionRisk(it) === 'unknown';
    if (d !== 'deny' && (risky || d === 'allow_always') && it.kind === 'approval') setConfirm(d);
    else send({ decision: d }, d);
  };
  const openPane = () => item.pane && navigate({ name: 'pane', host: item.host_id, pane: item.pane.id, view: 'term' });
  const refresh = () => {
    app.resetAnswer(item.host_id, it.id);
    void app.conn(item.host_id)?.refresh().catch(() => {});
  };

  // Swipe (low/medium approvals only).
  const canSwipe = it.kind === 'approval' && swipeAllowed(it) && !disabled;
  const [dx, setDx] = useState(0);
  const start = useRef<{ x: number; y: number; id: number } | null>(null);
  const horizontal = useRef(false);
  const onDown = (e: ReactPointerEvent) => {
    if (!canSwipe || (e.target as HTMLElement).closest('button,input,textarea,a,pre')) return;
    start.current = { x: e.clientX, y: e.clientY, id: e.pointerId };
    horizontal.current = false;
  };
  const onMove = (e: ReactPointerEvent) => {
    const s = start.current;
    if (!s || s.id !== e.pointerId) return;
    const mx = e.clientX - s.x;
    const my = e.clientY - s.y;
    if (!horizontal.current) {
      if (Math.abs(mx) > 12 && Math.abs(mx) > Math.abs(my) * 1.5) {
        horizontal.current = true;
        (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
      } else if (Math.abs(my) > 12) {
        start.current = null;
        return;
      } else return;
    }
    setDx(mx);
  };
  const onUp = () => {
    const was = dx;
    start.current = null;
    setDx(0);
    if (!horizontal.current) return;
    if (was > SWIPE_AT) send({ decision: 'allow' }, 'allow');
    else if (was < -SWIPE_AT) send({ decision: 'deny' }, 'deny');
  };

  return (
    <div className={cx('relative', leaving && 'animate-leave')}>
      {dx !== 0 && (
        <div
          className={cx(
            'absolute inset-0 flex items-center rounded-2xl px-5 text-sm font-semibold',
            dx > 0 ? 'justify-start bg-ok/20 text-ok' : 'justify-end bg-danger/20 text-danger',
          )}
        >
          {dx > 0 ? t.inbox.allow : t.inbox.deny}
        </div>
      )}
      <Card className={cx('relative animate-in touch-pan-y p-3.5', it.kind === 'approval' && interactionRisk(it) === 'high' && 'border-danger/40')}>
        <div
          onPointerDown={onDown}
          onPointerMove={onMove}
          onPointerUp={onUp}
          onPointerCancel={onUp}
          style={dx ? { transform: `translateX(${dx}px)`, transition: 'none' } : { transition: 'transform .15s' }}
          className="space-y-2.5"
        >
          <CardHeader item={item} showHost={showHost} />
          <div className="text-[15px] font-medium leading-snug">{it.title}</div>
          {it.kind === 'approval' && <ActionPreview it={it} />}
          {it.kind === 'question' && <QuestionBody it={it} disabled={disabled} locked={locked} onSubmit={(c, tx) => send({ ...(c ? { choices: c } : {}), ...(tx ? { text: tx } : {}) }, 'answer')} />}
          {it.kind === 'plan_review' && <PlanBody it={it} disabled={disabled} locked={locked} onApprove={() => send({ decision: 'allow' }, 'approve')} onChanges={(tx) => send({ decision: 'deny', text: tx }, 'changes')} />}
          {it.kind === 'notice' && it.body_md && <Markdown text={it.body_md} className="text-sm" />}

          {preselect && !locked && <Notice tone="info">{t.inbox.preselected(preselect === 'allow' ? t.inbox.allow : t.inbox.deny)}</Notice>}
          {!it.answerable && it.status === 'open' && <Notice tone="warn" action={<Button size="sm" onClick={openPane}>{t.inbox.openPane}</Button>}>{t.inbox.notAnswerable}</Notice>}
          {it.answerable && !ctx.online && !locked && <Notice tone="warn">{t.inbox.hostOffline}</Notice>}
          {it.answerable && ctx.scope === 'view' && <Notice>{t.inbox.readOnly}</Notice>}

          {it.kind === 'approval' && !locked && it.answerable && ctx.scope !== 'view' && (
            <div className="flex gap-2 pt-0.5">
              <Button
                variant="outline"
                className={cx('flex-1', preselect === 'deny' && 'border-danger! text-danger!')}
                disabled={disabled}
                icon={<X className="size-4" />}
                onClick={() => decide('deny')}
              >
                {t.inbox.deny}
              </Button>
              <Button variant="outline" className="flex-1" disabled={disabled} onClick={() => decide('allow_always')}>
                {t.inbox.allowAlways}
              </Button>
              <Button
                variant="ok"
                className={cx('flex-1', preselect === 'allow' && 'outline-2 outline-offset-2 outline-ok')}
                disabled={disabled}
                icon={<Check className="size-4" />}
                onClick={() => decide('allow')}
              >
                {t.inbox.allow}
              </Button>
            </div>
          )}
          {canSwipe && !locked && <div className="text-center text-[11px] text-faint">{t.inbox.swipeHint}</div>}
          {view && <DeliveryLine view={view} error={local?.error} onOpenPane={openPane} onRefresh={refresh} />}
        </div>
      </Card>
      <Sheet open={confirm !== null} onClose={() => setConfirm(null)} title={t.inbox.confirmTitle}>
        <div className="space-y-3">
          <p className="text-sm">{t.inbox.confirmHigh(it.action?.command ? `\`${it.action.command}\`` : it.title)}</p>
          {it.action?.command && <pre className="term max-h-48 overflow-auto whitespace-pre-wrap break-all rounded-xl border border-border p-2 text-[12px]">{it.action.command}</pre>}
          <div className="flex gap-2">
            <Button className="flex-1" variant="outline" onClick={() => setConfirm(null)}>
              {t.cancel}
            </Button>
            <Button
              className="flex-1"
              variant="danger"
              onClick={() => {
                const d = confirm;
                setConfirm(null);
                if (d) send({ decision: d }, d);
              }}
            >
              {confirm === 'allow_always' ? t.inbox.allowAlways : t.inbox.confirmAllow}
            </Button>
          </div>
        </div>
      </Sheet>
    </div>
  );
}

function QuestionBody({ it, disabled, locked, onSubmit }: { it: Interaction; disabled: boolean; locked: boolean; onSubmit(choices: Record<string, string[]> | null, text: string | null): void }) {
  const [sel, setSel] = useState<Record<string, string[]>>({});
  const [text, setText] = useState('');
  const free = it.questions.some((q) => q.allow_free_text) || it.questions.length === 0;
  const anyChoice = Object.values(sel).some((v) => v.length > 0);
  const toggle = (qid: string, oid: string, multi: boolean) => {
    setSel((s) => {
      const cur = s[qid] ?? [];
      const next = multi ? (cur.includes(oid) ? cur.filter((x) => x !== oid) : [...cur, oid]) : cur[0] === oid ? [] : [oid];
      return { ...s, [qid]: next };
    });
  };
  return (
    <div className="space-y-3">
      {it.body_md && <Markdown text={it.body_md} className="text-sm" />}
      {it.questions.map((q) => (
        <div key={q.id} className="space-y-1.5">
          {q.header && <div className="text-[11px] font-semibold uppercase tracking-wide text-faint">{q.header}</div>}
          {q.prompt && q.prompt !== it.title && <div className="text-sm">{q.prompt}</div>}
          {q.multi && <div className="text-[11px] text-faint">{t.inbox.multiHint}</div>}
          <div className="flex flex-col gap-1.5">
            {q.options.map((o) => {
              const on = (sel[q.id] ?? []).includes(o.id);
              return (
                <button
                  key={o.id}
                  type="button"
                  disabled={disabled}
                  onClick={() => toggle(q.id, o.id, q.multi)}
                  aria-pressed={on}
                  className={cx(
                    'rounded-xl border px-3 py-2 text-left text-sm disabled:opacity-50',
                    on ? 'border-accent bg-accent/10' : 'border-border bg-bg',
                  )}
                >
                  <div className="font-medium">{o.label}</div>
                  {o.description && <div className="text-[12px] text-muted">{o.description}</div>}
                </button>
              );
            })}
          </div>
        </div>
      ))}
      {free && !locked && (
        <textarea
          value={text}
          disabled={disabled}
          onChange={(e) => setText(e.target.value)}
          placeholder={t.inbox.answerPlaceholder}
          rows={2}
          className="w-full resize-none rounded-xl border border-border bg-bg px-3 py-2 text-[15px] placeholder:text-faint disabled:opacity-50"
        />
      )}
      {!locked && <Button
        variant="primary"
        block
        disabled={disabled || (!anyChoice && !text.trim())}
        onClick={() => onSubmit(anyChoice ? sel : null, text.trim() || null)}
      >
        {t.inbox.submit}
      </Button>}
    </div>
  );
}

function PlanBody({ it, disabled, locked, onApprove, onChanges }: { it: Interaction; disabled: boolean; locked: boolean; onApprove(): void; onChanges(text: string): void }) {
  const [asking, setAsking] = useState(false);
  const [text, setText] = useState('');
  const plan = useMemo(() => it.plan_md ?? it.body_md ?? '', [it.plan_md, it.body_md]);
  return (
    <div className="space-y-3">
      {plan && (
        <Collapsible max={260}>
          <Markdown text={plan} className="text-sm" />
        </Collapsible>
      )}
      {locked ? null : asking ? (
        <div className="space-y-2">
          <textarea
            autoFocus
            value={text}
            onChange={(e) => setText(e.target.value)}
            placeholder={t.inbox.changesPlaceholder}
            rows={3}
            className="w-full resize-none rounded-xl border border-border bg-bg px-3 py-2 text-[15px] placeholder:text-faint"
          />
          <div className="flex gap-2">
            <Button className="flex-1" variant="outline" onClick={() => setAsking(false)}>
              {t.cancel}
            </Button>
            <Button className="flex-1" variant="primary" disabled={disabled || !text.trim()} onClick={() => onChanges(text.trim())}>
              {t.send}
            </Button>
          </div>
        </div>
      ) : (
        <div className="flex gap-2">
          <Button className="flex-1" variant="outline" disabled={disabled} onClick={() => setAsking(true)}>
            {t.inbox.requestChanges}
          </Button>
          <Button className="flex-1" variant="ok" disabled={disabled} icon={<Check className="size-4" />} onClick={onApprove}>
            {t.inbox.approve}
          </Button>
        </div>
      )}
    </div>
  );
}
