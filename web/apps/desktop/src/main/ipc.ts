// IPC handlers (spec 16 §16.1). Every handler first checks that the call comes from the main frame
// of one of our windows showing the bundled app (`event.senderFrame` origin), then validates each
// argument (validate.ts). Results cross as `WireResult` so errors keep their kind.

import { BrowserWindow, clipboard, ipcMain, nativeTheme, shell, type IpcMainInvokeEvent, type WebContents } from 'electron';
import { parseLink, type HostRecord } from '@vibeke/core';
import { EVENT, INVOKE, type ChooseVibekeResult, type DesktopSettings, type LocalConnectResult, type RendererSettingsPatch, type WireResult } from '../shared/contract';
import { toWire, type Engine } from './engine';
import * as v from './validate';
import type { Updates } from './updater';
import type { DraftStore } from './drafts';
import type { Windows } from './windows';

export interface IpcDeps {
  updates: Updates;
  drafts: DraftStore;
  engine: Engine;
  /** A window starts / stops receiving one host's events. */
  setHostEvents(wc: WebContents, hostId: string, on: boolean): void;
  windows: Windows;
  trusted(): readonly string[];
  settings(): DesktopSettings;
  /** Renderer-settable keys only (validated by `settingsPatch`). */
  updateSettings(patch: RendererSettingsPatch): DesktopSettings;
  connectLocal(deviceName: string): Promise<LocalConnectResult>;
  startLocalGateway(): Promise<WireResult<null>>;
  /** Native picker in main + validation; the renderer supplies no path. */
  chooseVibeke(win: BrowserWindow | null): Promise<ChooseVibekeResult>;
  resetVibeke(): DesktopSettings;
  /** Native folder picker in main; null when canceled. */
  pickDirectory(win: BrowserWindow | null, defaultPath?: string): Promise<string | null>;
  onPrefsChanged(hostId: string): void;
  onTheme(theme: 'system' | 'light' | 'dark'): void;
  log(msg: string): void;
}

const ok = <T>(value: T): WireResult<T> => ({ ok: true, value });
const err = (e: unknown): WireResult<never> => ({ ok: false, error: toWire(e) });

export function registerIpc(d: IpcDeps): void {
  /** Sender check + argument validation wrapper. */
  const handle = (channel: string, f: (e: IpcMainInvokeEvent, ...args: unknown[]) => unknown) => {
    ipcMain.handle(channel, async (e, ...args) => {
      const frame = e.senderFrame;
      if (!frame || frame !== e.sender.mainFrame || !v.isTrustedUrl(frame.url, d.trusted()) || !d.windows.isOurs(e.sender)) {
        d.log(`ipc: refused ${channel} from ${frame?.url ?? '(no frame)'}`);
        throw new Error('forbidden');
      }
      return f(e, ...args);
    });
  };

  const draftKey = (host: unknown, pane: unknown) => {
    if (typeof host !== 'string' || typeof pane !== 'string' || !/^[a-zA-Z0-9_.:-]{1,160}$/.test(host) || !/^[a-zA-Z0-9_.:-]{1,160}$/.test(pane)) throw new Error('Invalid draft scope');
    return JSON.stringify([host, pane]);
  };
  handle(INVOKE.draftGet, async (_e, host, pane) => {
    try { draftKey(host, pane); return ok(await d.drafts.get(host as string, pane as string)); } catch (e) { return err(e); }
  });
  handle(INVOKE.draftSet, async (_e, host, pane, text) => {
    draftKey(host, pane);
    try {
      if (typeof text !== 'string') throw new Error('Draft is too large');
      await d.drafts.set(host as string, pane as string, text);
      return ok(null);
    } catch (e) { return err(e); }
  });
  for (const [channel, action] of [
    [INVOKE.updatesGet, () => d.updates.snapshot()],
    [INVOKE.updatesCheck, () => d.updates.check()],
    [INVOKE.updatesDownload, () => d.updates.download()],
    [INVOKE.updatesInstall, async () => { await d.drafts.flush(); d.updates.install(); }],
  ] as const) {
    handle(channel, async (_e, ...args) => {
      if (args.length) throw new Error('Update actions take no arguments');
      try { return ok((await action()) ?? null); } catch (e) { return err(e); }
    });
  }

  handle(INVOKE.engineStart, async () => {
    try {
      await d.engine.start();
      const p = d.engine.fullPatch();
      return ok({ fingerprint: d.engine.fingerprint, hosts: p.changed, version: p.version });
    } catch (e) {
      return err(e);
    }
  });

  handle(INVOKE.request, async (_e, host, method, params, opts) => {
    const h = v.hostId(host);
    const m = v.method(method);
    const p = v.params(params);
    const o = v.requestOpts(opts);
    try {
      const r = await d.engine.request(h, m, p, o);
      if (m === 'prefs.set') d.onPrefsChanged(h);
      return ok(r);
    } catch (e) {
      return err(e);
    }
  });

  handle(INVOKE.refresh, async (_e, host) => {
    try {
      await d.engine.refresh(v.hostId(host));
      return ok(null);
    } catch (e) {
      return err(e);
    }
  });

  handle(INVOKE.reconnect, (_e, host) => {
    d.engine.reconnect(host === null ? null : v.hostId(host));
    return ok(null);
  });

  handle(INVOKE.remove, async (_e, host) => {
    try {
      const id = v.hostId(host);
      await d.drafts.removeHost(id);
      await d.engine.remove(id);
      return ok(null);
    } catch (e) {
      return err(e);
    }
  });

  handle(INVOKE.pair, async (e, token, link, name) => {
    const tok = v.pairToken(token);
    const text = v.linkText(link);
    const deviceName = v.shortText(name, 'device name', 64).trim() || 'Desktop';
    try {
      const parsed = parseLink(text);
      const rec: HostRecord = await d.engine.pair(parsed, deviceName, (fingerprint) => {
        if (!e.sender.isDestroyed()) e.sender.send(EVENT.pairPending, { token: tok, fingerprint });
      });
      return ok(rec);
    } catch (x) {
      return err(x);
    }
  });

  handle(INVOKE.localConnect, async (_e, name) => {
    const deviceName = v.shortText(name, 'device name', 64).trim() || 'Desktop';
    return d.connectLocal(deviceName);
  });

  handle(INVOKE.localStartGateway, () => d.startLocalGateway());

  handle(INVOKE.chooseVibeke, async (e) => ok(await d.chooseVibeke(BrowserWindow.fromWebContents(e.sender))));

  handle(INVOKE.resetVibeke, () => ok(d.resetVibeke()));

  handle(INVOKE.pickDirectory, async (e, defaultPath) => {
    const start = v.defaultDirectory(defaultPath);
    return ok(await d.pickDirectory(BrowserWindow.fromWebContents(e.sender), start));
  });

  handle(INVOKE.ready, (e) => {
    d.windows.markReady(e.sender);
    return ok(null);
  });

  handle(INVOKE.hostEvents, (e, host, on) => {
    d.setHostEvents(e.sender, v.hostId(host), v.flag(on, 'host events flag'));
    return ok(null);
  });

  handle(INVOKE.openExternal, async (_e, url) => {
    await shell.openExternal(v.externalUrl(url));
    return ok(null);
  });

  handle(INVOKE.clipboardWrite, (_e, text) => {
    clipboard.writeText(v.shortText(text, 'clipboard text', 10 * 1024 * 1024));
    return ok(null);
  });

  handle(INVOKE.clipboardRead, async () => ok((await clipboard.readText()).slice(0, 1024 * 1024)));

  handle(INVOKE.settingsGet, () => ok(d.settings()));

  handle(INVOKE.settingsSet, (_e, patch) => {
    try {
      return ok(d.updateSettings(v.settingsPatch(patch)));
    } catch (x) {
      return err(x);
    }
  });

  handle(INVOKE.theme, (_e, theme) => {
    const t = v.theme(theme);
    nativeTheme.themeSource = t;
    d.onTheme(t);
    return ok(null);
  });

  handle(INVOKE.window, (e, op) => {
    const w = v.windowOp(op);
    const win = BrowserWindow.fromWebContents(e.sender);
    switch (w.op) {
      case 'pop-out':
        d.windows.popOutPane(w.host, w.pane);
        break;
      case 'open-main':
        d.windows.showMain(w.hash);
        break;
      case 'quick':
        d.windows.toggleQuick();
        break;
      case 'close':
        if (win === d.windows.quick) d.windows.hideQuick();
        else win?.close();
        break;
    }
    return ok(null);
  });
}
