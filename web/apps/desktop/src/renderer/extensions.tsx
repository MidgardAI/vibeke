// Desktop-only UI plugged into the shared app (UiExtensions): "Connect to this Mac" on the pairing
// screen, a Desktop settings group, and palette commands.

import { useEffect, useState, type ReactNode } from 'react';
import { CheckCircle2, Laptop, Terminal } from 'lucide-react';
import { Button, Card, Notice, SectionLabel, Toggle, navigate, useApp, usePrefs, type ShellCommand, type UiExtensions } from '@vibeke/ui';
import { EVENT, INVOKE, type BootInfo, type Bridge, type ChooseVibekeResult, type DesktopSettings, type LocalConnectResult, type RendererSettingsPatch, type WireResult } from '../shared/contract';
import { acceleratorFromEvent, acceleratorLabel } from '../shared/accelerator';
import { call } from './remote';

type Phase =
  | { k: 'idle' }
  | { k: 'busy'; what: 'pair' | 'gateway' }
  | { k: 'error'; code: string; message: string }
  | { k: 'done'; host: string };

function LocalConnect({ bridge, boot }: { bridge: Bridge; boot: BootInfo }) {
  const app = useApp();
  const prefs = usePrefs();
  const [phase, setPhase] = useState<Phase>({ k: 'idle' });
  const mac = boot.platform === 'darwin';
  const where = mac ? 'this Mac' : 'this computer';

  const connect = async () => {
    setPhase({ k: 'busy', what: 'pair' });
    const r = (await bridge.invoke(INVOKE.localConnect, prefs.deviceName || app.platform.defaultDeviceName)) as LocalConnectResult;
    if (r.ok) {
      app.haptic('success');
      setPhase({ k: 'done', host: r.record.name });
    } else setPhase({ k: 'error', code: r.code, message: r.message });
  };
  const startGateway = async () => {
    setPhase({ k: 'busy', what: 'gateway' });
    const r = (await bridge.invoke(INVOKE.localStartGateway)) as WireResult<null>;
    if (r.ok) return connect();
    setPhase({ k: 'error', code: (r.error as { code?: string }).code ?? 'gateway_failed', message: (r.error as { message: string }).message });
  };
  const choose = async () => {
    const r = await call<ChooseVibekeResult>(bridge, INVOKE.chooseVibeke).catch((e: Error) => ({ ok: false as const, canceled: false as const, message: e.message }));
    if (r.ok) return void connect();
    if (!r.canceled) setPhase({ k: 'error', code: 'cli_invalid', message: r.message });
  };
  const needsChoice = phase.k === 'error' && (phase.code === 'cli_not_found' || phase.code === 'cli_untrusted' || phase.code === 'cli_invalid');

  if (phase.k === 'done') {
    return (
      <Card className="space-y-3 p-4 text-center">
        <CheckCircle2 className="mx-auto size-10 text-ok" />
        <div className="font-medium">Connected to {phase.host}</div>
        <Button variant="primary" block onClick={() => navigate({ name: 'home' })}>
          Open Vibeke
        </Button>
      </Card>
    );
  }
  return (
    <Card className="space-y-3 p-4">
      <div className="flex items-start gap-3">
        <Laptop className="mt-0.5 size-6 shrink-0 text-accent" aria-hidden />
        <div className="min-w-0 flex-1">
          <div className="font-medium">Connect to {where}</div>
          <div className="text-[13px] text-muted">
            Uses the Vibeke gateway on {where} directly over a private local socket: no relay, no QR code, still end-to-end encrypted.
          </div>
        </div>
      </div>
      {phase.k === 'error' && (
        <Notice
          tone={phase.code === 'gateway_not_running' || phase.code === 'server_not_running' ? 'warn' : 'danger'}
          action={
            phase.code === 'gateway_not_running' ? (
              <Button size="sm" variant="outline" onClick={() => void startGateway()}>
                Start gateway
              </Button>
            ) : needsChoice ? (
              <Button size="sm" variant="outline" onClick={() => void choose()}>
                Choose…
              </Button>
            ) : undefined
          }
        >
          {phase.message}
          {phase.code === 'gateway_not_running' && <div className="mt-1 text-[12px] text-muted">Or run <code>vibeke gateway run</code> in a terminal.</div>}
          {phase.code === 'cli_not_found' && <div className="mt-1 text-[12px] text-muted">Install Vibeke, or choose the <code>vibeke</code> executable.</div>}
        </Notice>
      )}
      <Button
        variant="primary"
        size="lg"
        block
        busy={phase.k === 'busy'}
        icon={<Terminal className="size-5" />}
        onClick={() => void connect()}
      >
        {phase.k === 'busy' ? (phase.what === 'gateway' ? 'Starting the gateway…' : 'Connecting…') : `Connect to ${where}`}
      </Button>
    </Card>
  );
}

function Row({ label, hint, children }: { label: ReactNode; hint?: ReactNode; children?: ReactNode }) {
  return (
    <div className="flex min-h-12 items-center gap-3 px-4 py-2">
      <div className="min-w-0 flex-1">
        <div className="text-[15px]">{label}</div>
        {hint && <div className="text-[12px] text-muted">{hint}</div>}
      </div>
      {children}
    </div>
  );
}

function useDesktopSettings(bridge: Bridge, initial: DesktopSettings) {
  const [s, setS] = useState(initial);
  const [err, setErr] = useState<string | null>(null);
  useEffect(() => {
    void call<DesktopSettings>(bridge, INVOKE.settingsGet).then(setS, () => {});
    return bridge.on(EVENT.settings, (v) => setS(v as DesktopSettings));
  }, [bridge]);
  const patch = async (p: RendererSettingsPatch) => {
    setErr(null);
    try {
      setS(await call<DesktopSettings>(bridge, INVOKE.settingsSet, p));
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  /** The executable is chosen in main's native picker (validated there), never typed here. */
  const choose = async () => {
    setErr(null);
    try {
      const r = await call<ChooseVibekeResult>(bridge, INVOKE.chooseVibeke);
      if (!r.ok && !r.canceled) setErr(r.message);
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  const reset = async () => {
    setErr(null);
    try {
      setS(await call<DesktopSettings>(bridge, INVOKE.resetVibeke));
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  return { s, patch, choose, reset, err };
}

function ShortcutField({ value, mac, onChange }: { value: string; mac: boolean; onChange(v: string): void }) {
  const [recording, setRecording] = useState(false);
  useEffect(() => {
    if (!recording) return;
    const onKey = (e: KeyboardEvent) => {
      e.preventDefault();
      e.stopPropagation();
      if (e.key === 'Escape') return setRecording(false);
      const acc = acceleratorFromEvent(e, mac);
      if (acc) {
        setRecording(false);
        onChange(acc);
      }
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  }, [recording, mac, onChange]);
  return (
    <div className="flex items-center gap-2">
      <Button size="sm" variant={recording ? 'primary' : 'outline'} aria-label="Record shortcut" onClick={() => setRecording(!recording)}>
        {recording ? 'Press keys…' : value ? <kbd className="font-sans">{acceleratorLabel(value, mac)}</kbd> : 'Off'}
      </Button>
      {value && !recording && (
        <Button size="sm" variant="ghost" onClick={() => onChange('')}>
          Clear
        </Button>
      )}
    </div>
  );
}

function DesktopSettingsSection({ bridge, boot }: { bridge: Bridge; boot: BootInfo }) {
  const { s, patch, choose, reset, err } = useDesktopSettings(bridge, boot.settings);
  const mac = boot.platform === 'darwin';
  return (
    <section>
      <SectionLabel>Desktop</SectionLabel>
      <div className="inset-group divide-y divide-border border-y border-border bg-surface">
        <Row label="Menu-bar shortcut" hint="Opens the menu-bar agents and approvals from anywhere.">
          <ShortcutField value={s.shortcut} mac={mac} onChange={(v) => void patch({ shortcut: v })} />
        </Row>
        <Row label="Notifications" hint="New requests and stopped agents, per each host’s privacy level and Do Not Disturb.">
          <Toggle label="Notifications" checked={s.notifications} onChange={(v) => void patch({ notifications: v })} />
        </Row>
        <Row label="Open at login" hint="Starts in the menu bar, without a window.">
          <Toggle label="Open at login" checked={s.openAtLogin} onChange={(v) => void patch({ openAtLogin: v })} />
        </Row>
        {mac && (
          <Row label="Show in Dock" hint="Off: menu bar only.">
            <Toggle label="Show in Dock" checked={s.showDock} onChange={(v) => void patch({ showDock: v })} />
          </Row>
        )}
        <Row label="vibeke command" hint={s.vibekePath || 'Found automatically in the usual install locations (~/.local/bin, Homebrew, ~/.cargo/bin).'}>
          <div className="flex gap-1">
            <Button size="sm" variant="outline" onClick={() => void choose()}>
              Choose…
            </Button>
            {s.vibekePath && (
              <Button size="sm" variant="ghost" onClick={() => void reset()}>
                Reset
              </Button>
            )}
          </div>
        </Row>
        {err && (
          <div className="px-4 py-2">
            <Notice tone="danger">{err}</Notice>
          </div>
        )}
      </div>
    </section>
  );
}

export function extensions(bridge: Bridge, boot: BootInfo): UiExtensions {
  const where = boot.platform === 'darwin' ? 'This Mac' : 'This Computer';
  return {
    pairPanel: () => <LocalConnect bridge={bridge} boot={boot} />,
    settingsSection: () => <DesktopSettingsSection bridge={bridge} boot={boot} />,
    commands: (): ShellCommand[] => [
      { id: 'quick', title: 'Agents and approvals', hint: boot.settings.shortcut ? acceleratorLabel(boot.settings.shortcut, boot.platform === 'darwin') : undefined, keywords: 'menu bar popover agents tray', run: () => void bridge.invoke(INVOKE.window, { op: 'quick' }) },
      { id: 'local', title: `Connect to ${where}…`, keywords: 'local gateway pair', run: () => navigate({ name: 'pair', d: null }) },
    ],
  };
}

