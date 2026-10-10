// Settings (spec 16 §9.1): Appearance, Device, Alerts (push, per-host notify prefs, DND), Quick
// replies, System (hosts, devices, pairing, connection info), Sharing & handoff (invitations,
// peers, invited devices, handoff prefs; spec 16 §15.2–§15.4), About (app origin + build).

import { useEffect, useState, type ReactNode } from 'react';
import { BellRing, Download, Link2, Lock, Plus, Server, Smartphone, Trash2, Users } from 'lucide-react';
import {
  RpcError,
  displayName,
  hostKind,
  paneTitle,
  transportOf,
  type DeviceInfo,
  type DevicePrefs,
  type HandoffPrefs,
  type HostState,
  type InvitationInfo,
  type InvitedDeviceInfo,
  type PeerInfo,
} from '@vibeke/core';
import { useAllHosts, useApp, useHosts, useNow, usePrefs } from '../app/hooks';
import { Button, Card, Dot, Notice, SectionLabel, Segmented, Spinner, TextField, Toggle } from '../components/ui';
import { LANGUAGES, t } from '../i18n';
import { AGENT_VIEWS } from '../lib/agent-view';
import { CACHE_TTL_MIN_RANGE, cacheTtlMs } from '../lib/cache-clock';
import { errorMessage } from '../lib/answer';
import { clockTime, whenText } from '../lib/format';
import { DEFAULT_QUICK_REPLIES, harnessLabel } from '../lib/harness';
import { useStore } from '../lib/store';
import { navigate } from '../router';
import { ReceiveHandoff } from './share';
import { UpdateControls } from '../components/updates';

function Row({ label, hint, children }: { label: ReactNode; hint?: ReactNode; children?: ReactNode }) {
  return (
    <div className="flex min-h-12 items-center gap-3 px-4 py-2">
      <div className="min-w-0 flex-1">
        <div className="text-base">{label}</div>
        {hint && <div className="text-xs text-muted">{hint}</div>}
      </div>
      {children}
    </div>
  );
}

function Group({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section>
      <SectionLabel>{title}</SectionLabel>
      <div className="inset-group divide-y divide-border border-y border-border bg-surface">{children}</div>
    </section>
  );
}

export function SettingsScreen() {
  const app = useApp();
  const prefs = usePrefs();
  const hosts = useHosts();
  const allHosts = useAllHosts();
  const Ext = app.platform.extensions?.settingsSection;
  const touch = typeof window !== 'undefined' && !!window.matchMedia?.('(pointer: coarse)').matches;
  return (
    <div className="pb-10">
      <Group title={t.settings.appearance}>
        <Row label={t.settings.theme}>
          <Segmented
            label={t.settings.theme}
            value={prefs.theme}
            onChange={(v) => app.prefs.patch({ theme: v })}
            options={(['system', 'light', 'dark'] as const).map((v) => ({ value: v, label: t.settings.themes[v]! }))}
          />
        </Row>
        <Row
          label={t.settings.agentView}
          hint={
            <>
              {t.settings.agentViewHint}
              {Object.keys(prefs.agentViews).length > 0 && (
                <>
                  {' '}
                  {t.settings.agentViewOverrides(Object.keys(prefs.agentViews).length)} ·{' '}
                  <button type="button" className="vk-focus rounded-sm text-accent hover:underline" onClick={() => app.prefs.patch({ agentViews: {} })}>
                    {t.settings.agentViewReset}
                  </button>
                </>
              )}
            </>
          }
        >
          <Segmented
            label={t.settings.agentView}
            value={prefs.agentView}
            onChange={(v) => app.prefs.patch({ agentView: v })}
            options={AGENT_VIEWS.map((v) => ({ value: v, label: t.settings.agentViews[v]! }))}
          />
        </Row>
        <Row label={t.settings.termFont}>
          <div className="flex items-center gap-2">
            <Button size="sm" variant="outline" onClick={() => app.prefs.patch({ termFont: Math.max(8, prefs.termFont - 1) })}>
              −
            </Button>
            <span className="w-7 text-center tabular-nums">{prefs.termFont}</span>
            <Button size="sm" variant="outline" onClick={() => app.prefs.patch({ termFont: Math.min(24, prefs.termFont + 1) })}>
              +
            </Button>
          </div>
        </Row>
        <Row label={t.settings.beltSize}>
          <Segmented
            label={t.settings.beltSize}
            value={prefs.beltSize}
            onChange={(v) => app.prefs.patch({ beltSize: v })}
            options={(['s', 'm', 'l'] as const).map((v) => ({ value: v, label: t.settings.sizes[v]! }))}
          />
        </Row>
      </Group>

      {Ext && <Ext />}

      <Group title={t.settings.cacheTitle}>
        <div className="px-4 pt-2 text-xs text-muted">{t.settings.cacheHint}</div>
        {(['claude', 'codex'] as const).map((h) => (
          <CacheTtlRow key={h} harness={h} />
        ))}
      </Group>

      <Group title={t.settings.device}>
        <div className="px-4 py-2">
          <TextField label={t.settings.deviceName} value={prefs.deviceName} onChange={(e) => app.prefs.patch({ deviceName: e.target.value })} maxLength={64} />
        </div>
        {/* Phone/tablet-only behaviours: not shown where they cannot apply (desktop). */}
        {app.platform.haptics && (
          <Row label={t.settings.haptics}>
            <Toggle label={t.settings.haptics} checked={prefs.haptics} onChange={(v) => app.prefs.patch({ haptics: v })} />
          </Row>
        )}
        {touch && (
          <Row label={t.settings.zenLandscape}>
            <Toggle label={t.settings.zenLandscape} checked={prefs.zenLandscape} onChange={(v) => app.prefs.patch({ zenLandscape: v })} />
          </Row>
        )}
      </Group>

      <Alerts hosts={hosts.filter((h) => h.status !== 'expired')} />

      <Group title={t.settings.quickReplies}>
        <QuickReplies />
      </Group>

      <section>
        <SectionLabel right={<Button size="sm" variant="ghost" icon={<Plus className="size-4" />} onClick={() => navigate({ name: 'pair', d: null })}>{t.settings.pairAnother}</Button>}>
          {t.settings.hosts}
        </SectionLabel>
        <div className="space-y-3 px-3">
          {allHosts.length === 0 && <div className="px-1 text-sm text-muted">{t.settings.noHosts}</div>}
          {allHosts.map((h) => (
            <HostCard key={h.record.host_id} h={h} />
          ))}
        </div>
      </section>

      <SharingGroup hosts={hosts} />

      <About />
    </div>
  );
}

function PushControl() {
  const app = useApp();
  const state = useStore(app.push);
  const n = app.platform.notifications;
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const perm = n?.permission() ?? 'unsupported';
  if (app.platform.localAlerts) return <Row label={t.settings.localAlerts} hint={t.settings.localAlertsHint} />;
  if (state === 'unsupported' || !n) return <Row label={t.settings.push} hint={t.settings.pushUnsupported} />;
  if (n.needsInstallForPush()) return <Row label={t.settings.push} hint={t.settings.iosInstall} />;
  const on = state === 'on';
  return (
    <>
      <Row label={t.settings.push} hint={perm === 'denied' ? t.settings.pushDenied : (err ?? undefined)}>
        <Button
          size="sm"
          variant={on ? 'outline' : 'primary'}
          busy={busy}
          disabled={perm === 'denied'}
          icon={<BellRing className="size-4" />}
          onClick={async () => {
            setBusy(true);
            setErr(null);
            try {
              if (on) await app.push.disable();
              else {
                // Permission must come from this tap (iOS).
                const p = await n.requestPermission();
                if (p !== 'granted') throw new Error(t.settings.pushDenied);
                await app.push.enable();
              }
            } catch (e) {
              setErr(errorMessage(e));
            } finally {
              setBusy(false);
            }
          }}
        >
          {on ? t.settings.pushDisable : t.settings.pushEnable}
        </Button>
      </Row>
    </>
  );
}

function CacheTtlRow({ harness }: { harness: string }) {
  const app = useApp();
  const prefs = usePrefs();
  const minutes = Math.round((cacheTtlMs(harness, prefs.cacheTtl) ?? 0) / 60_000);
  const custom = prefs.cacheTtl[harness] !== undefined;
  const set = (n: number | null) => {
    const next = { ...prefs.cacheTtl };
    if (n === null) delete next[harness];
    else next[harness] = Math.min(CACHE_TTL_MIN_RANGE.max, Math.max(CACHE_TTL_MIN_RANGE.min, n));
    app.prefs.patch({ cacheTtl: next });
  };
  const step = (d: number) => set(minutes + d);
  return (
    <Row label={harnessLabel(harness)} hint={custom ? <button type="button" className="vk-focus rounded-sm text-accent hover:underline" onClick={() => set(null)}>{t.settings.cacheReset}</button> : undefined}>
      <div className="flex items-center gap-2">
        <Button size="sm" variant="outline" aria-label={t.settings.cacheLess} disabled={minutes <= CACHE_TTL_MIN_RANGE.min} onClick={() => step(minutes > 10 ? -5 : -1)}>
          −
        </Button>
        <span className="w-14 text-center tabular-nums">{t.settings.cacheMinutes(minutes)}</span>
        <Button size="sm" variant="outline" aria-label={t.settings.cacheMore} disabled={minutes >= CACHE_TTL_MIN_RANGE.max} onClick={() => step(minutes >= 10 ? 5 : 1)}>
          +
        </Button>
      </div>
    </Row>
  );
}

function Alerts({ hosts }: { hosts: readonly HostState[] }) {
  return (
    <Group title={t.settings.alerts}>
      <PushControl />
      {hosts.map((h) => (
        <HostAlertPrefs key={h.record.host_id} h={h} showName={hosts.length > 1} />
      ))}
    </Group>
  );
}

function HostAlertPrefs({ h, showName }: { h: HostState; showName: boolean }) {
  const app = useApp();
  const online = h.status === 'online';
  const conn = app.conn(h.record.host_id);
  const [prefs, setPrefs] = useState<DevicePrefs | null>(null);
  const [dnd, setDnd] = useState<number>(0);
  const [err, setErr] = useState<string | null>(null);
  const [tested, setTested] = useState(false);

  useEffect(() => {
    if (!online || !conn) return;
    conn.request('prefs.get', {}).then(
      (r) => {
        setPrefs({
          privacy: (r.device.privacy as DevicePrefs['privacy']) ?? 'summary',
          notify_input: r.device.notify_input ?? true,
          notify_done: r.device.notify_done ?? false,
          // Only sent back when the host reports it, so an older host never sees an unknown key.
          ...(typeof r.device.notify_cache_cold === 'boolean' ? { notify_cache_cold: r.device.notify_cache_cold } : {}),
        });
        setDnd(r.host.dnd_until ?? 0);
      },
      (e) => setErr(errorMessage(e)),
    );
  }, [online, h.record.host_id]);

  const save = async (p: DevicePrefs) => {
    const prev = prefs;
    setPrefs(p);
    try {
      await conn!.request('prefs.set', { device: p });
    } catch (e) {
      setPrefs(prev);
      setErr(errorMessage(e));
    }
  };
  const setDndFor = async (secs: number) => {
    const until = secs ? Math.floor(app.platform.clock.now() / 1000) + secs : 0;
    try {
      await conn!.request('prefs.set', { host: { dnd_until: until } });
      setDnd(until);
    } catch (e) {
      setErr(errorMessage(e));
    }
  };
  const nowS = Math.floor(app.platform.clock.now() / 1000);
  const name = h.info?.host_name ?? h.record.name;

  if (!online) return <Row label={showName ? name : t.settings.perHost} hint={t.conn.hostOffline} />;
  if (!prefs) return <Row label={showName ? name : t.settings.perHost}>{err ? <span className="text-xs text-danger">{err}</span> : <Spinner />}</Row>;
  return (
    <div className="space-y-0">
      {showName && <div className="px-4 pt-3 text-sm font-semibold">{name}</div>}
      <Row label={t.settings.notifyInput}>
        <Toggle label={t.settings.notifyInput} checked={prefs.notify_input} onChange={(v) => void save({ ...prefs, notify_input: v })} />
      </Row>
      <Row label={t.settings.notifyDone}>
        <Toggle label={t.settings.notifyDone} checked={prefs.notify_done} onChange={(v) => void save({ ...prefs, notify_done: v })} />
      </Row>
      <Row label={t.settings.notifyCacheCold} hint={t.settings.notifyCacheColdHint}>
        <Toggle label={t.settings.notifyCacheCold} checked={prefs.notify_cache_cold ?? false} onChange={(v) => void save({ ...prefs, notify_cache_cold: v })} />
      </Row>
      <Row label={t.settings.privacy} hint={t.settings.privacyHint}>
        <Segmented
          label={t.settings.privacy}
          value={prefs.privacy}
          onChange={(v) => void save({ ...prefs, privacy: v })}
          options={(['full', 'summary', 'minimal'] as const).map((v) => ({ value: v, label: t.settings.privacyLevels[v]! }))}
        />
      </Row>
      <Row label={t.settings.dnd} hint={dnd > nowS ? t.settings.dndUntil(clockTime(dnd * 1000)) : undefined}>
        <div className="flex flex-wrap justify-end gap-1">
          {(
            [
              [0, t.settings.dndOff],
              [1800, t.settings.dnd30],
              [3600, t.settings.dnd1h],
              [14400, t.settings.dnd4h],
            ] as const
          ).map(([s, label]) => (
            <Button key={s} size="sm" variant={(s === 0 ? dnd <= nowS : false) ? 'primary' : 'outline'} onClick={() => void setDndFor(s)}>
              {label}
            </Button>
          ))}
        </div>
      </Row>
      {!app.platform.localAlerts && <Row label={t.settings.pushTest}>
        <Button
          size="sm"
          variant="outline"
          onClick={async () => {
            try {
              await conn!.request('push.test', {});
              setTested(true);
            } catch (e) {
              setErr(errorMessage(e));
            }
          }}
        >
          {tested ? t.settings.pushTestSent : t.settings.pushTest}
        </Button>
      </Row>}
      {err && <div className="px-4 pb-2 text-xs text-danger">{err}</div>}
    </div>
  );
}

function QuickReplies() {
  const app = useApp();
  const prefs = usePrefs();
  const [harness, setHarness] = useState('*');
  const list = prefs.quickReplies[harness] ?? DEFAULT_QUICK_REPLIES[harness] ?? DEFAULT_QUICK_REPLIES['*']!;
  const [text, setText] = useState(list.join('\n'));
  useEffect(() => setText(list.join('\n')), [harness]);
  return (
    <div className="space-y-2 px-4 py-3">
      <Segmented
        label={t.settings.quickReplies}
        value={harness}
        onChange={setHarness}
        options={[
          { value: '*', label: 'Default' },
          { value: 'claude', label: 'Claude' },
          { value: 'codex', label: 'Codex' },
          { value: 'pi', label: 'pi' },
        ]}
      />
      <textarea
        value={text}
        rows={5}
        onChange={(e) => setText(e.target.value)}
        onBlur={() =>
          app.prefs.patch({
            quickReplies: { ...prefs.quickReplies, [harness]: text.split('\n').map((s) => s.trim()).filter(Boolean) },
          })
        }
        className="w-full resize-none rounded-xl border border-border bg-bg px-3 py-2 font-mono text-sm"
      />
      <div className="text-xs text-muted">{t.settings.quickHint}</div>
    </div>
  );
}

function HostCard({ h }: { h: HostState }) {
  const app = useApp();
  const [armed, setArmed] = useState(false);
  const [devices, setDevices] = useState<DeviceInfo[] | null>(null);
  const [armedDev, setArmedDev] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const conn = app.conn(h.record.host_id);
  const online = h.status === 'online';
  const scope = h.info?.scope ?? h.record.scope;
  const kind = hostKind(h.record);
  const now = useNow(30_000);

  useEffect(() => {
    // Share devices may not list devices.
    if (!online || !conn || kind !== 'device') return;
    conn.request('devices.list', {}).then(
      (r) => setDevices(r.devices),
      (e) => setErr(errorMessage(e)),
    );
  }, [online, h.record.host_id]);

  const tone = h.status === 'online' ? 'ok' : h.status === 'connecting' ? 'warn' : h.status === 'expired' ? 'muted' : 'danger';
  const statusText =
    h.status === 'online'
      ? t.conn.online
      : h.status === 'expired'
        ? t.conn.expired
      : h.status === 'ticket_expired'
        ? t.conn.ticketExpired
      : h.status === 'revoked'
        ? t.conn.revoked
        : h.status === 'unauthorized'
          ? t.conn.unauthorized
          : h.status === 'incompatible'
            ? t.conn.incompatible
            : h.status === 'connecting'
              ? t.conn.connecting
              : t.conn.hostOffline;

  return (
    <Card className="overflow-hidden">
      <div className="flex items-center gap-2 px-4 pt-3">
        <Dot tone={tone} />
        <div className="min-w-0 flex-1 font-medium">{h.info?.host_name ?? h.record.name}</div>
        <span className="text-xs text-muted">{statusText}</span>
      </div>
      {kind !== 'device' && (
        <div className="px-4 pt-1 text-xs text-muted">
          {[
            t.settings.sharedWithYou,
            h.record.label,
            h.record.until !== undefined
              ? (h.record.until * 1000 <= now ? t.settings.expiredAt : t.settings.expiresAt)(whenText(h.record.until * 1000, now))
              : null,
          ]
            .filter(Boolean)
            .join(' · ')}
        </div>
      )}
      <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 px-4 py-2 text-xs">
        <dt className="text-muted">{t.settings.scope}</dt>
        <dd>{scope}</dd>
        <dt className="text-muted">{transportOf(h.record.relay) === 'local' ? t.settings.transport : 'Relay'}</dt>
        <dd className="truncate font-mono" title={h.record.relay}>{transportOf(h.record.relay) === 'local' ? t.pair.thisComputer : h.record.relay}</dd>
        <dt className="text-muted">Host id</dt>
        <dd className="truncate font-mono">{h.record.host_id}</dd>
        {h.info && (
          <>
            <dt className="text-muted">{t.settings.server}</dt>
            <dd>{h.info.server_version ?? '—'}</dd>
            <dt className="text-muted">{t.settings.gateway}</dt>
            <dd>{(h.info as { gateway_version?: string }).gateway_version ?? '—'}</dd>
            <dt className="text-muted">{t.settings.features}</dt>
            <dd>{h.info.features.filter(Boolean).join(', ')}</dd>
          </>
        )}
        {h.error && h.status !== 'online' && (
          <>
            <dt className="text-muted">Error</dt>
            <dd className="text-danger">
              {h.error}
              {h.closeCode ? ` (${h.closeCode})` : ''}
            </dd>
          </>
        )}
      </dl>
      {devices && (
        <div className="border-t border-border px-4 py-2">
          <div className="mb-1 text-2xs font-semibold uppercase tracking-wide text-faint">{t.settings.devices}</div>
          {devices.map((d) => (
            <div key={d.id} className="flex min-h-10 items-center gap-2 text-sm">
              <Smartphone className="size-4 text-muted" />
              <div className="min-w-0 flex-1">
                <div className="truncate">
                  {d.name} {d.this && <span className="text-muted">({t.settings.thisDevice})</span>}
                </div>
                <div className="font-mono text-2xs text-faint">
                  {d.fingerprint} · {d.scope} · {d.platform}
                </div>
                {d.kind && d.kind !== 'device' && <DeviceShareInfo d={d} now={now} h={h} />}
              </div>
              {!d.this && scope === 'full' && (
                <Button
                  size="sm"
                  variant={armedDev === d.id ? 'danger' : 'outline'}
                  onClick={async () => {
                    if (armedDev !== d.id) return setArmedDev(d.id);
                    try {
                      await conn!.request('devices.revoke', { device: d.id });
                      setDevices((l) => l?.filter((x) => x.id !== d.id) ?? null);
                    } catch (e) {
                      setErr(errorMessage(e));
                    }
                    setArmedDev(null);
                  }}
                >
                  {armedDev === d.id ? t.settings.revokeConfirm : t.settings.revoke}
                </Button>
              )}
            </div>
          ))}
        </div>
      )}
      {err && <div className="px-4 pb-2 text-xs text-danger">{err}</div>}
      <div className="flex gap-2 border-t border-border px-4 py-2">
        {h.status === 'ticket_expired' && (
          <Button size="sm" variant="outline" onClick={() => navigate({ name: 'pair', d: null })}>
            {t.pair.title}
          </Button>
        )}
        {h.status !== 'online' && h.status !== 'expired' && h.status !== 'ticket_expired' && (
          <Button size="sm" variant="outline" onClick={() => app.conn(h.record.host_id)?.reconnectNow()}>
            {t.retry}
          </Button>
        )}
        <span className="flex-1" />
        <Button
          size="sm"
          variant={armed ? 'danger' : 'ghost'}
          icon={<Trash2 className="size-4" />}
          onClick={() => {
            if (!armed) return setArmed(true);
            void app.forgetHost(h.record.host_id);
          }}
        >
          {armed ? t.settings.forgetConfirm : t.settings.forget}
        </Button>
      </div>
    </Card>
  );
}

function DeviceShareInfo({ d, now, h }: { d: DeviceInfo; now: number; h: HostState }) {
  const exp = d.expires_at ?? null;
  const pane = d.limit?.pane ? h.dashboard?.panes.find((p) => p.id === d.limit!.pane) : undefined;
  const ws = d.limit?.workspace ? h.dashboard?.workspaces.find((w) => w.id === d.limit!.workspace) : undefined;
  const parts = [
    t.settings.kinds[d.kind ?? 'device'] ?? d.kind,
    d.limit?.pane
      ? t.settings.limitPane(pane ? paneTitle(pane) : d.limit.pane)
      : d.limit?.workspace
        ? t.settings.limitWorkspace(ws ? displayName(ws) : d.limit.workspace)
        : null,
    exp !== null ? (exp * 1000 <= now ? t.settings.expiredAt : t.settings.expiresAt)(whenText(exp * 1000, now)) : null,
  ].filter(Boolean);
  return <div className={exp !== null && exp * 1000 <= now ? 'text-2xs text-danger' : 'text-2xs text-accent'}>{parts.join(' · ')}</div>;
}

/** Spec 16 §15.2–§15.4: per own host, invitations, peers, invited devices and handoff prefs. */
function SharingGroup({ hosts }: { hosts: readonly HostState[] }) {
  const own = hosts.filter((h) => hostKind(h.record) === 'device' && (h.info?.scope ?? h.record.scope) === 'full');
  if (own.length === 0) return null;
  return (
    <Group title={t.settings.sharing}>
      {own.map((h) => {
        const name = h.info?.host_name ?? h.record.name;
        return (
          <div key={h.record.host_id}>
            {own.length > 1 && <div className="px-4 pt-3 text-sm font-semibold">{name}</div>}
            {h.status === 'online' ? <HostSharing h={h} hostName={name} /> : <Row label={name} hint={t.conn.hostOffline} />}
          </div>
        );
      })}
    </Group>
  );
}

/** The host does not know the method (an older gateway or server). */
const unknownMethod = (e: unknown): boolean => e instanceof RpcError && (e.kind === 'method_not_found' || e.code === -32601);

interface SharingData {
  invitations: InvitationInfo[] | null;
  devices: InvitedDeviceInfo[] | null;
  peers: PeerInfo[] | null;
  prefs: HandoffPrefs | null;
}

function HostSharing({ h, hostName }: { h: HostState; hostName: string }) {
  const app = useApp();
  const id = h.record.host_id;
  const conn = app.conn(id);
  const now = useNow(30_000);
  const [data, setData] = useState<SharingData | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [armed, setArmed] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  const load = async () => {
    if (!conn) return;
    const [sl, pl, hp] = await Promise.allSettled([conn.request('share.list', {}), conn.request('peer.list', {}), conn.request('handoff.prefs', {})]);
    const failed = [sl, pl, hp].find((r): r is PromiseRejectedResult => r.status === 'rejected' && !unknownMethod(r.reason));
    setErr(failed ? errorMessage(failed.reason) : null);
    setData({
      invitations: sl.status === 'fulfilled' ? (sl.value.invitations ?? []) : null,
      devices: sl.status === 'fulfilled' ? (sl.value.devices ?? []) : null,
      peers: pl.status === 'fulfilled' ? (pl.value.peers ?? []) : null,
      prefs: hp.status === 'fulfilled' ? hp.value : null,
    });
  };

  useEffect(() => {
    void load();
  }, [id]);

  /** Destructive actions ask twice (tap, then tap again). */
  const act = async (key: string, run: () => Promise<unknown>) => {
    if (armed !== key) return setArmed(key);
    setArmed(null);
    setBusy(key);
    setErr(null);
    try {
      await run();
      await load();
    } catch (e) {
      setErr(errorMessage(e));
    } finally {
      setBusy(null);
    }
  };

  const setAlwaysAsk = async (v: boolean) => {
    if (!conn || !data?.prefs) return;
    const prev = data.prefs;
    setData({ ...data, prefs: { ...prev, always_ask: v } });
    try {
      const r = await conn.request('handoff.prefs', { always_ask: v });
      setData((d) => (d ? { ...d, prefs: r } : d));
    } catch (e) {
      setData((d) => (d ? { ...d, prefs: prev } : d));
      setErr(errorMessage(e));
    }
  };

  const until = (s: number | null) => (s === null ? null : (s * 1000 <= now ? t.settings.expiredAt : t.settings.expiresAt)(whenText(s * 1000, now)));
  const revokeButton = (key: string, label: string, run: () => Promise<unknown>) => (
    <Button size="sm" variant={armed === key ? 'danger' : 'outline'} busy={busy === key} onClick={() => void act(key, run)}>
      {armed === key ? t.settings.revokeConfirm : label}
    </Button>
  );

  return (
    <div className="divide-y divide-border">
      {data?.prefs && (
        <Row label={t.settings.alwaysAsk} hint={t.settings.alwaysAskHint}>
          <Toggle label={t.settings.alwaysAsk} checked={data.prefs.always_ask} onChange={(v) => void setAlwaysAsk(v)} />
        </Row>
      )}
      <Row label={t.settings.incomingHandoffs} hint={t.settings.incomingHandoffsHint(hostName)}>
        <Button size="sm" variant="outline" onClick={() => navigate({ name: 'handoffs', host: id, id: null })}>
          {t.open}
        </Button>
      </Row>

      <ReceiveHandoff hostId={id} hostName={hostName} onCreated={() => void load()} />

      {!data && !err && (
        <div className="flex justify-center py-3">
          <Spinner />
        </div>
      )}

      {data?.invitations && data.invitations.length > 0 && (
        <List title={t.settings.invitations}>
          {data.invitations.map((i) => (
            <Item
              key={i.id}
              icon={<Link2 className="size-4 text-muted" />}
              title={i.label || (t.settings.inviteKinds[i.kind] ?? i.kind)}
              sub={[t.settings.inviteKinds[i.kind] ?? i.kind, i.scope, t.settings.openBy(whenText(i.link_expires_at * 1000, now))].join(' · ')}
            >
              {revokeButton(`inv:${i.id}`, t.settings.cancelInvite, () => conn!.request('share.revoke', { id: i.id }))}
            </Item>
          ))}
        </List>
      )}

      {data?.peers && (
        <List title={t.settings.peers} hint={t.settings.peersHint(hostName)}>
          {data.peers.length === 0 && <div className="py-1 text-xs text-muted">{t.settings.noPeers}</div>}
          {data.peers.map((p) => (
            <Item
              key={p.id}
              icon={p.owner === 'teammate' ? <Users className="size-4 text-muted" /> : <Server className="size-4 text-muted" />}
              title={p.name}
              sub={[p.owner === 'teammate' ? t.settings.teammate : t.settings.ownHost, until(p.expires_at)].filter(Boolean).join(' · ')}
              danger={p.expired}
            >
              {revokeButton(`peer:${p.id}`, t.remove, () => conn!.request('peer.remove', { id: p.id }))}
            </Item>
          ))}
        </List>
      )}

      {data?.devices && data.devices.length > 0 && (
        <List title={t.settings.invitedDevices}>
          {data.devices.map((d) => {
            const user = d.sender?.user;
            const who = [user?.name, user?.email ? `<${user.email}>` : null].filter(Boolean).join(' ');
            const from = who || d.sender?.host_name || d.name;
            return (
              <Item
                key={d.id}
                icon={d.kind === 'peer' ? <Server className="size-4 text-muted" /> : <Smartphone className="size-4 text-muted" />}
                title={from}
                sub={[
                  t.settings.kinds[d.kind] ?? d.kind,
                  d.owner === 'teammate' ? t.settings.teammate : d.owner === 'self' ? t.settings.ownHost : null,
                  d.sender?.host_name && d.sender.host_name !== from ? d.sender.host_name : null,
                  d.scope,
                  until(d.expires_at),
                ]
                  .filter(Boolean)
                  .join(' · ')}
                danger={d.expires_at !== null && d.expires_at * 1000 <= now}
              >
                {revokeButton(`dev:${d.id}`, t.settings.revoke, () => conn!.request('share.revoke', { id: d.id }))}
              </Item>
            );
          })}
        </List>
      )}

      {err && <div className="px-4 py-2 text-xs text-danger">{err}</div>}
    </div>
  );
}

function List({ title, hint, children }: { title: string; hint?: string; children: ReactNode }) {
  return (
    <div className="px-4 py-2">
      <div className="mb-1 text-2xs font-semibold uppercase tracking-wide text-faint">{title}</div>
      {hint && <div className="mb-1 text-xs text-muted">{hint}</div>}
      {children}
    </div>
  );
}

function Item({ icon, title, sub, danger, children }: { icon: ReactNode; title: string; sub: string; danger?: boolean; children?: ReactNode }) {
  return (
    <div className="flex min-h-10 items-center gap-2 text-sm">
      {icon}
      <div className="min-w-0 flex-1">
        <div className="truncate">{title}</div>
        <div className={danger ? 'text-2xs text-danger' : 'text-2xs text-muted'}>{sub}</div>
      </div>
      {children}
    </div>
  );
}

function About() {
  const app = useApp();
  const inst = app.platform.install;
  const [, force] = useState(0);
  useEffect(() => inst?.subscribe(() => force((x) => x + 1)), [inst]);
  const b = app.platform.build;
  return (
    <Group title={t.settings.about}>
      <Row label={t.language.label} hint={t.language.hint}>
        <Segmented
          label={t.language.label}
          value={app.prefs.get().language}
          onChange={(v) => app.prefs.patch({ language: v })}
          options={[{ value: 'system', label: t.language.system }, ...LANGUAGES.map((l) => ({ value: l, label: t.language.names[l] ?? l }))]}
        />
      </Row>
      {app.platform.updates && <div className="px-4 py-3"><UpdateControls /></div>}
      {inst?.canPrompt() && (
        <Row label={t.install.title} hint={t.install.body}>
          <Button size="sm" variant="primary" icon={<Download className="size-4" />} onClick={() => void inst.prompt()}>
            {t.install.action}
          </Button>
        </Row>
      )}
      {inst?.iosShareSheet && !inst.standalone && <div className="px-4 py-3"><Notice>{t.settings.iosInstall}</Notice></div>}
      <Row label={t.settings.appOrigin}>
        <span className="font-mono text-xs">{b.origin}</span>
      </Row>
      <Row label={t.settings.build}>
        <span className="font-mono text-xs">
          {b.version} · {b.hash}
        </span>
      </Row>
      <div className="px-4 py-3 text-xs text-muted">{t.settings.trustNote}</div>
      <Row label={t.settings.tour}>
        <Button size="sm" variant="outline" onClick={() => app.prefs.patch({ tourDone: false })}>
          {t.open}
        </Button>
      </Row>
      <Row label={t.settings.lock}>
        <Button size="sm" variant="outline" icon={<Lock className="size-4" />} onClick={() => app.locked.set(true)}>
          {t.settings.lock}
        </Button>
      </Row>
    </Group>
  );
}
