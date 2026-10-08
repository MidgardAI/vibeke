// Pairing (spec 16 §4): parse the link from the fragment, show host + this device's fingerprint
// ("check this matches your terminal"), run pair.claim, wait for the host's confirmation.
// A handoff invitation (§15.2) is not for this device: one of the user's hosts redeems it
// (`peer.redeem`), so that host can send work to the teammate's host directly.

import { useEffect, useMemo, useState } from 'react';
import { CheckCircle2, Link2, QrCode, Server, ShieldCheck } from 'lucide-react';
import { ChannelError, PairingError, hostKind, linkExpired, parseLink, transportOf, type PairingLink } from '@vibeke/core';
import { useAllHosts, useApp, usePrefs } from '../app/hooks';
import { QrScanner, qrScanSupported } from '../components/qr-scanner';
import { Button, Card, Dot, Notice, Spinner, TextField, cx } from '../components/ui';
import { t } from '../i18n';
import { errorMessage } from '../lib/answer';
import { clockTime, whenText } from '../lib/format';
import { isOwnFullHost } from '../lib/handoff-send';
import { navigate } from '../router';

type Phase =
  | { k: 'form' }
  | { k: 'connecting' }
  | { k: 'pending'; fingerprint: string }
  | { k: 'done'; host: string; kind: 'device' | 'share' }
  /** A handoff invitation redeemed by one of the user's hosts (`on`) for the teammate's `to`. */
  | { k: 'redeemed'; on: string; to: string }
  | { k: 'error'; message: string };

/** Map a pairing failure to the user-facing reason. */
export function pairingErrorMessage(e: unknown): string {
  if (e instanceof PairingError) {
    switch (e.code) {
      case 'expired':
        return t.pair.errors.expired;
      case 'rejected':
        return t.pair.errors.rejected;
      case 'fingerprint_mismatch':
        return t.pair.errors.mismatch;
      case 'timeout':
        return t.pair.errors.timeout;
      case 'channel': {
        const c = e.cause;
        if (c instanceof ChannelError) {
          if (c.code === 'unauthorized') return t.pair.errors.unauthorized;
          if (c.closeCode === 4404 || c.closeCode === 4408 || c.code === 'closed') return t.pair.errors.offline;
        }
        return t.pair.errors.offline;
      }
      default:
        return `${t.pair.errors.generic} ${e.message}`;
    }
  }
  return `${t.pair.errors.generic} ${(e as Error)?.message ?? ''}`;
}

export function PairScreen({ d }: { d: string | null }) {
  const app = useApp();
  const prefs = usePrefs();
  const hosts = useAllHosts();
  const [raw, setRaw] = useState<string | null>(d);
  const [paste, setPaste] = useState('');
  const [scan, setScan] = useState(false);
  const [phase, setPhase] = useState<Phase>({ k: 'form' });
  const [name, setName] = useState(prefs.deviceName);

  // Keep the secret out of history once read.
  useEffect(() => {
    if (d) {
      setRaw(d);
      navigate({ name: 'pair', d: null }, { replace: true });
    }
  }, [d]);

  const parsed = useMemo((): { link: PairingLink | null; error: string | null } => {
    if (!raw) return { link: null, error: null };
    try {
      return { link: parseLink(raw), error: null };
    } catch {
      return { link: null, error: t.pair.errors.invalid };
    }
  }, [raw]);
  const link = parsed.link;
  const expired = link ? linkExpired(link, app.platform.clock.now()) : false;
  const existing = link ? hosts.find((h) => h.record.host_id === link.host) : undefined;
  const known = !!existing;
  const share = link?.share;
  // The host would keep both devices (each invitation gets its own key), but this app keeps one
  // record per host: accepting would swap full access for the invitation's. It isn't needed.
  const replacesOwn = share?.kind === 'share' && !!existing && hostKind(existing.record) === 'device';
  const handoff = share?.kind === 'handoff';
  const fp = app.deviceFingerprint();
  const local = link ? transportOf(link.relay) === 'local' : false;
  const unreachable = local && !app.platform.localTransport;
  const Panel = app.platform.extensions?.pairPanel;

  const start = async () => {
    if (!link) return;
    app.prefs.patch({ deviceName: name.trim() || app.platform.defaultDeviceName });
    setPhase({ k: 'connecting' });
    try {
      const rec = await app.pair(link, name.trim() || app.platform.defaultDeviceName, (f) => setPhase({ k: 'pending', fingerprint: f }));
      app.haptic('success');
      setPhase({ k: 'done', host: rec.name, kind: rec.kind ?? 'device' });
      setRaw(null);
    } catch (e) {
      app.haptic('error');
      setPhase({ k: 'error', message: pairingErrorMessage(e) });
    }
  };

  return (
    <div className="mx-auto max-w-md space-y-4 px-4 py-5">
      {phase.k === 'done' ? (
        <Card className="space-y-4 p-5 text-center">
          <CheckCircle2 className="mx-auto size-12 text-ok" />
          <div className="text-lg font-medium">
            {phase.kind === 'share' ? t.pair.doneShare(phase.host) : t.pair.done(phase.host)}
          </div>
          <Button variant="primary" block size="lg" onClick={() => navigate({ name: 'home' })}>
            {t.pair.openApp}
          </Button>
        </Card>
      ) : phase.k === 'redeemed' ? (
        <Card className="space-y-4 p-5 text-center">
          <CheckCircle2 className="mx-auto size-12 text-ok" />
          <div className="text-lg font-medium">{t.pair.redeemed(phase.on, phase.to)}</div>
          <div className="text-sm text-muted">{t.pair.redeemedHint}</div>
          <Button variant="primary" block size="lg" onClick={() => navigate({ name: 'home' })}>
            {t.done}
          </Button>
        </Card>
      ) : link ? (
        <Card className="space-y-4 p-4">
          {share && <ShareHeader link={link} />}
          {handoff && raw ? (
            <RedeemHandoff
              link={link}
              raw={raw}
              expired={expired}
              onDone={(on) => {
                setPhase({ k: 'redeemed', on, to: link.name });
                setRaw(null);
              }}
            />
          ) : (
            <>
            <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1.5 text-sm">
              <dt className="text-muted">{t.pair.hostName}</dt>
              <dd className="font-medium">{link.name}</dd>
              <dt className="text-muted">{local ? t.settings.transport : t.pair.relay}</dt>
              <dd className="truncate font-mono text-xs" title={link.relay}>{local ? t.pair.thisComputer : link.relay}</dd>
            </dl>
            <div className="text-xs text-muted">{t.pair.expires(clockTime(link.exp * 1000))}</div>
            {replacesOwn ? <Notice tone="warn">{t.pair.replacesOwn}</Notice> : known && <Notice>{t.pair.alreadyPaired}</Notice>}
            {expired && <Notice tone="danger">{t.pair.errors.expired}</Notice>}
            {unreachable && <Notice tone="warn">{t.pair.localOnly}</Notice>}

            {/* Invitations and local links are bearer links: nobody confirms a fingerprint on the host side. */}
            {!share && !local && (
              <div className="rounded-2xl border border-border bg-bg p-4 text-center">
                <div className="text-xs uppercase tracking-wide text-faint">{t.pair.checkFingerprint}</div>
                <div className="mt-1 font-mono text-3xl font-semibold tracking-wider">{phase.k === 'pending' ? phase.fingerprint : fp}</div>
              </div>
            )}

            {phase.k === 'form' || phase.k === 'error' ? (
              <>
                <TextField label={t.pair.deviceName} value={name} onChange={(e) => setName(e.target.value)} maxLength={64} />
                {phase.k === 'error' && <Notice tone="danger">{phase.message}</Notice>}
                <Button variant="primary" size="lg" block disabled={expired || unreachable || replacesOwn} icon={<ShieldCheck className="size-5" />} onClick={() => void start()}>
                  {share ? t.pair.accept : t.pair.start}
                </Button>
              </>
            ) : (
              <div className="flex items-start gap-3 text-sm">
                <Spinner className="mt-0.5" />
                <span>{share ? t.pair.joining : phase.k === 'connecting' ? t.pair.connecting : t.pair.waitingConfirm}</span>
              </div>
            )}
            </>
          )}
        </Card>
      ) : (
        <>
        {Panel && <Panel />}
        <Card className="space-y-4 p-4">
          <p className="text-sm text-muted">{t.pair.intro}</p>
          {parsed.error && <Notice tone="danger">{parsed.error}</Notice>}
          {scan ? (
            <QrScanner
              onResult={(text) => {
                setScan(false);
                setRaw(text);
              }}
            />
          ) : (
            qrScanSupported() && app.platform.qrScan !== false && (
              <Button block variant="secondary" icon={<QrCode className="size-5" />} onClick={() => setScan(true)}>
                {t.pair.scan}
              </Button>
            )
          )}
          <form
            className="space-y-2"
            onSubmit={(e) => {
              e.preventDefault();
              setRaw(paste.trim());
            }}
          >
            <TextField label={t.pair.paste} placeholder={t.pair.pastePlaceholder} value={paste} onChange={(e) => setPaste(e.target.value)} autoCapitalize="off" autoCorrect="off" />
            <Button type="submit" block variant="primary" icon={<Link2 className="size-4" />} disabled={!paste.trim()}>
              {t.open}
            </Button>
          </form>
        </Card>
        </>
      )}
    </div>
  );
}

/** "the maintainer's devbox shared samplehub with you · view-only · until 16:00" / handoff invitation. */
function ShareHeader({ link }: { link: PairingLink }) {
  const app = useApp();
  const s = link.share!;
  const now = app.platform.clock.now();
  if (s.kind === 'handoff') {
    return (
      <div className="space-y-1">
        <div className="text-base font-semibold">{t.pair.handoffFrom(link.name)}</div>
        <div className="text-sm text-muted">{t.pair.handoffWhat}</div>
      </div>
    );
  }
  const what = s.label || (s.limit?.pane ? t.pair.shareWhatPane : t.pair.shareWhatWorkspace);
  return (
    <div className="space-y-1">
      <div className="text-base font-semibold">{t.pair.shareFrom(link.name, what)}</div>
      <div className="text-sm text-muted">
        {[t.pair.shareScope[s.scope] ?? s.scope, t.pair.shareUntil(whenText(s.until * 1000, now))].join(' · ')}
      </div>
    </div>
  );
}

/**
 * "Accept on which of your hosts?": a handoff invitation lets a host send work to the teammate's
 * host. The chosen host redeems it (`peer.redeem`); this app is not paired with theirs.
 */
function RedeemHandoff({ link, raw, expired, onDone }: { link: PairingLink; raw: string; expired: boolean; onDone(hostName: string): void }) {
  const app = useApp();
  const hosts = useAllHosts().filter((h) => isOwnFullHost(h) && h.record.host_id !== link.host);
  const [chosen, setChosen] = useState<string | null>(null);
  const [shareUser, setShareUser] = useState(false);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const firstOnline = hosts.find((h) => h.status === 'online')?.record.host_id ?? null;
  const pick = chosen ?? firstOnline;
  const target = hosts.find((h) => h.record.host_id === pick);
  const name = (h: (typeof hosts)[number]) => h.info?.host_name ?? h.record.name;

  const redeem = async () => {
    if (!target) return;
    const conn = app.conn(target.record.host_id);
    if (!conn) return;
    setBusy(true);
    setErr(null);
    try {
      await conn.request('peer.redeem', { link: raw.trim(), share_user: shareUser }, { timeoutMs: 90_000 });
      app.haptic('success');
      onDone(name(target));
    } catch (e) {
      app.haptic('error');
      setErr(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  if (hosts.length === 0) return <Notice tone="warn">{t.pair.redeemNoHost}</Notice>;
  return (
    <div className="space-y-3">
      <div className="text-xs text-muted">{t.pair.expires(clockTime(link.exp * 1000))}</div>
      <div className="text-sm font-medium">{t.pair.redeemOn}</div>
      <div role="radiogroup" aria-label={t.pair.redeemOn} className="space-y-1.5">
        {hosts.map((h) => {
          const online = h.status === 'online';
          const on = h.record.host_id === pick;
          return (
            <button
              key={h.record.host_id}
              type="button"
              role="radio"
              aria-checked={on}
              disabled={!online}
              onClick={() => setChosen(h.record.host_id)}
              className={cx('flex min-h-12 w-full items-center gap-3 rounded-xl border px-3 text-left disabled:opacity-40', on ? 'border-border-strong bg-selected' : 'border-border')}
            >
              <Server className="size-5 text-muted" />
              <span className="min-w-0 flex-1 truncate text-base">{name(h)}</span>
              {!online && <span className="text-xs text-muted">{t.handoff.offline}</span>}
              <Dot tone={online ? 'ok' : 'muted'} />
            </button>
          );
        })}
      </div>
      <label className="flex items-start gap-2 text-sm">
        <input type="checkbox" className="mt-1" checked={shareUser} onChange={(e) => setShareUser(e.target.checked)} />
        <span>
          {t.pair.shareUser}
          <span className="block text-xs text-muted">{t.pair.shareUserHint(link.name)}</span>
        </span>
      </label>
      {expired && <Notice tone="danger">{t.pair.errors.expired}</Notice>}
      {err && <Notice tone="danger">{err}</Notice>}
      <Button variant="primary" size="lg" block busy={busy} disabled={expired || !target || target.status !== 'online'} icon={<ShieldCheck className="size-5" />} onClick={() => void redeem()}>
        {target ? t.pair.redeemOnHost(name(target)) : t.pair.accept}
      </Button>
    </div>
  );
}
