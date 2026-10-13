// `#/continue?d=…` (spec 20 §4, §7.2): a session someone sent as a link. The screen shows the brief
// carried in the link, lists the user's own hosts with full access, and asks one of them to take
// it (`handoff.link.redeem`): that host downloads, decrypts and checks the bundle, then it shows
// up as an incoming handoff.

import { useEffect, useMemo, useState } from 'react';
import { Server, ShieldCheck } from 'lucide-react';
import { continueExpired, parseContinueLink, type ContinueLink, type HandoffLinkJob } from '@vibeke/core';
import { useAllHosts, useApp } from '../app/hooks';
import { Button, Card, Dot, Notice, Spinner, cx } from '../components/ui';
import { t } from '../i18n';
import { errorMessage } from '../lib/answer';
import { byteSize } from '../lib/format';
import { isOwnFullHost } from '../lib/handoff-send';
import { navigate } from '../router';

const POLL_MS = 1000;
/** A redeem job that stays unfinished this long is reported as stuck. */
const MAX_WAIT_MS = 30 * 60_000;

/** The incoming handoff id from a finished redeem job. */
export function incomingId(job: HandoffLinkJob): string | null {
  const i = job.incoming;
  if (typeof i === 'string' && i) return i;
  if (i && typeof i === 'object' && typeof i.id === 'string') return i.id;
  return null;
}

const jobError = (job: HandoffLinkJob): string => {
  const e = job.error;
  if (!e) return '';
  return typeof e === 'string' ? e : (e.message ?? e.code ?? '');
};

export function ContinueScreen({ d }: { d: string | null }) {
  const app = useApp();
  const parsed = useMemo((): { link: ContinueLink | null; raw: string | null } => {
    if (!d) return { link: null, raw: null };
    try {
      return { link: parseContinueLink(d), raw: d };
    } catch {
      return { link: null, raw: null };
    }
  }, [d]);
  const hosts = useAllHosts().filter(isOwnFullHost);
  const [chosen, setChosen] = useState<string | null>(null);
  const [job, setJob] = useState<{ host: string; state: HandoffLinkJob | null } | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const link = parsed.link;

  // Follow the job until it is done or failed, then open the incoming handoff.
  useEffect(() => {
    if (!job || job.state?.state === 'done' || job.state?.state === 'failed') return;
    const id = job.state?.job;
    if (!id) return;
    let live = true;
    const started = app.platform.clock.now();
    const tick = async () => {
      if (!live) return;
      try {
        const s = await app.conn(job.host)?.request('handoff.link.status', { job: id });
        if (!live || !s) return;
        setJob({ host: job.host, state: s });
        if (s.state === 'done') {
          const inc = incomingId(s);
          navigate({ name: 'handoffs', host: job.host, id: inc }, { replace: true });
          return;
        }
        if (s.state === 'failed') return setErr(t.continueLink.failed(jobError(s) || '?'));
      } catch {
        // keep polling: the host may be reconnecting
      }
      if (app.platform.clock.now() - started > MAX_WAIT_MS) return setErr(t.continueLink.failed('timeout'));
      setTimeout(() => void tick(), POLL_MS);
    };
    const h = setTimeout(() => void tick(), POLL_MS);
    return () => {
      live = false;
      clearTimeout(h);
    };
  }, [app, job?.host, job?.state?.job]);

  if (!link) {
    return (
      <div className="mx-auto max-w-md px-4 py-5">
        <Notice tone="danger">{t.continueLink.invalid}</Notice>
      </div>
    );
  }
  const expired = continueExpired(link, app.platform.clock.now());
  const firstOnline = hosts.find((h) => h.status === 'online')?.record.host_id ?? null;
  const pick = chosen ?? firstOnline;
  const target = hosts.find((h) => h.record.host_id === pick);
  const nameOf = (h: (typeof hosts)[number]) => h.info?.host_name ?? h.record.name;
  const b = link.brief;

  const redeem = async () => {
    if (!target || !parsed.raw) return;
    setErr(null);
    const host = target.record.host_id;
    setJob({ host, state: null });
    try {
      const r = await app.conn(host)!.request('handoff.link.redeem', { link: `${window.location.origin}/#/continue?d=${parsed.raw}` }, { timeoutMs: 60_000 });
      setJob({ host, state: { job: r.job, state: 'downloading' } });
    } catch (e) {
      setJob(null);
      setErr(errorMessage(e));
    }
  };

  const running = !!job && !err;
  return (
    <div className="mx-auto max-w-md space-y-4 px-4 py-5">
      <Card className="space-y-3 p-4">
        <div className="text-base font-semibold">{b.title || t.continueLink.title}</div>
        {b.from && <div className="text-sm text-muted">{t.continueLink.from(b.from)}</div>}
        <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1.5 text-sm">
          {b.repo && (
            <>
              <dt className="text-muted">{t.continueLink.repo}</dt>
              <dd className="truncate font-medium">{b.repo}</dd>
            </>
          )}
          {b.branch && (
            <>
              <dt className="text-muted">{t.continueLink.branch}</dt>
              <dd className="truncate font-mono text-xs">{b.branch}</dd>
            </>
          )}
          {b.harness && (
            <>
              <dt className="text-muted">{t.continueLink.harness}</dt>
              <dd>{b.harness}</dd>
            </>
          )}
          <dt className="text-muted">{t.continueLink.size}</dt>
          <dd>{byteSize(link.size)}</dd>
        </dl>
        {b.last && (
          <div>
            <div className="text-xs text-muted">{t.continueLink.last}</div>
            <p className="mt-1 whitespace-pre-wrap rounded-lg border border-border bg-bg p-2 text-sm">{b.last}</p>
          </div>
        )}
        <p className="text-xs text-muted">{t.continueLink.excluded}</p>
      </Card>
      {expired ? (
        <Notice tone="danger">{t.continueLink.expired}</Notice>
      ) : hosts.length === 0 ? (
        <Notice tone="warn">{t.continueLink.noHost}</Notice>
      ) : (
        <Card className="space-y-3 p-4">
          <div className="text-sm font-medium">{t.continueLink.chooseHost}</div>
          <div role="radiogroup" aria-label={t.continueLink.chooseHost} className="space-y-1.5">
            {hosts.map((h) => {
              const online = h.status === 'online';
              const on = h.record.host_id === pick;
              return (
                <button
                  key={h.record.host_id}
                  type="button"
                  role="radio"
                  aria-checked={on}
                  disabled={!online || running}
                  onClick={() => setChosen(h.record.host_id)}
                  className={cx('flex min-h-12 w-full items-center gap-3 rounded-xl border px-3 text-left disabled:opacity-40', on ? 'border-border-strong bg-selected' : 'border-border')}
                >
                  <Server className="size-5 text-muted" />
                  <span className="min-w-0 flex-1 truncate text-base">{nameOf(h)}</span>
                  {!online && <span className="text-xs text-muted">{t.handoff.offline}</span>}
                  <Dot tone={online ? 'ok' : 'muted'} />
                </button>
              );
            })}
          </div>
          {err && <Notice tone="danger">{err}</Notice>}
          {running ? (
            <div className="flex items-center gap-3 text-sm">
              <Spinner />
              <span>
                {t.continueLink.states[job?.state?.state ?? 'downloading'] ?? job?.state?.state}
                {typeof job?.state?.progress === 'number' && job.state.progress >= 0 && job.state.progress <= 1 ? ` ${Math.round(job.state.progress * 100)}%` : ''}
              </span>
            </div>
          ) : (
            <Button variant="primary" size="lg" block disabled={!target || target.status !== 'online'} icon={<ShieldCheck className="size-5" />} onClick={() => void redeem()}>
              {target ? t.continueLink.continueOn(nameOf(target)) : t.continueLink.title}
            </Button>
          )}
        </Card>
      )}
    </div>
  );
}
