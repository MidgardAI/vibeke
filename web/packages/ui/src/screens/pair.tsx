// Pairing (spec 16 §4): parse the link from the fragment, show host + this device's fingerprint
// ("check this matches your terminal"), run pair.claim, wait for the host's confirmation.

import { useEffect, useMemo, useState } from 'react';
import { CheckCircle2, Link2, QrCode, ShieldCheck } from 'lucide-react';
import { ChannelError, PairingError, hostKind, linkExpired, parseLink, transportOf, type PairingLink } from '@vibeke/core';
import { useAllHosts, useApp, usePrefs } from '../app/hooks';
import { QrScanner, qrScanSupported } from '../components/qr-scanner';
import { Button, Card, Notice, Spinner, TextField } from '../components/ui';
import { t } from '../i18n';
import { clockTime, whenText } from '../lib/format';
import { navigate } from '../router';

type Phase =
  | { k: 'form' }
  | { k: 'connecting' }
  | { k: 'pending'; fingerprint: string }
  | { k: 'done'; host: string; kind: 'device' | 'share' | 'handoff' }
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
  const replacesOwn = !!share && !!existing && hostKind(existing.record) === 'device';
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
            {phase.kind === 'share' ? t.pair.doneShare(phase.host) : phase.kind === 'handoff' ? t.pair.doneHandoff(phase.host) : t.pair.done(phase.host)}
          </div>
          <Button variant="primary" block size="lg" onClick={() => navigate(phase.kind === 'handoff' ? { name: 'settings' } : { name: 'home' })}>
            {phase.kind === 'handoff' ? t.settings.title : t.pair.openApp}
          </Button>
        </Card>
      ) : link ? (
        <Card className="space-y-4 p-4">
          {share && <ShareHeader link={link} />}
          <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1.5 text-sm">
            <dt className="text-muted">{t.pair.hostName}</dt>
            <dd className="font-medium">{link.name}</dd>
            <dt className="text-muted">{local ? t.settings.transport : t.pair.relay}</dt>
            <dd className="truncate font-mono text-[12px]" title={link.relay}>{local ? t.pair.thisComputer : link.relay}</dd>
          </dl>
          <div className="text-[12px] text-muted">{t.pair.expires(clockTime(link.exp * 1000))}</div>
          {replacesOwn ? <Notice tone="warn">{t.pair.replacesOwn}</Notice> : known && <Notice>{t.pair.alreadyPaired}</Notice>}
          {expired && <Notice tone="danger">{t.pair.errors.expired}</Notice>}
          {unreachable && <Notice tone="warn">{t.pair.localOnly}</Notice>}

          {/* Invitations and local links are bearer links: nobody confirms a fingerprint on the host side. */}
          {!share && !local && (
            <div className="rounded-2xl border border-border bg-bg p-4 text-center">
              <div className="text-[12px] uppercase tracking-wide text-faint">{t.pair.checkFingerprint}</div>
              <div className="mt-1 font-mono text-3xl font-semibold tracking-wider">{phase.k === 'pending' ? phase.fingerprint : fp}</div>
            </div>
          )}

          {phase.k === 'form' || phase.k === 'error' ? (
            <>
              <TextField label={t.pair.deviceName} value={name} onChange={(e) => setName(e.target.value)} maxLength={64} />
              {phase.k === 'error' && <Notice tone="danger">{phase.message}</Notice>}
              <Button variant="primary" size="lg" block disabled={expired || unreachable} icon={<ShieldCheck className="size-5" />} onClick={() => void start()}>
                {share ? t.pair.accept : t.pair.start}
              </Button>
            </>
          ) : (
            <div className="flex items-start gap-3 text-sm">
              <Spinner className="mt-0.5" />
              <span>{share ? t.pair.joining : phase.k === 'connecting' ? t.pair.connecting : t.pair.waitingConfirm}</span>
            </div>
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
        <div className="text-[15px] font-semibold">{t.pair.handoffFrom(link.name)}</div>
        <div className="text-[13px] text-muted">{t.pair.handoffWhat}</div>
      </div>
    );
  }
  const what = s.label || (s.limit?.pane ? t.pair.shareWhatPane : t.pair.shareWhatWorkspace);
  return (
    <div className="space-y-1">
      <div className="text-[15px] font-semibold">{t.pair.shareFrom(link.name, what)}</div>
      <div className="text-[13px] text-muted">
        {[t.pair.shareScope[s.scope] ?? s.scope, t.pair.shareUntil(whenText(s.until * 1000, now))].join(' · ')}
      </div>
    </div>
  );
}
